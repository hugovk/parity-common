// Copyright 2019 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

// TODO: docs
// #![deny(missing_docs)]

use std::path::Path;
use std::{io, fs, mem};
use std::ops::DerefMut;

use kvdb::{KeyValueDB, DBTransaction, DBValue, DBOp};
use lmdb::{
	Environment, Database as LmdbDatabase, DatabaseFlags,
	Transaction, RoTransaction, RwTransaction,
	Iter as LmdbIter, Cursor, RoCursor,
	Error, WriteFlags,
};

use log::{warn, debug};

use owning_ref::OwningHandle;
use fs_swap::{swap, swap_nonatomic};
use parking_lot::{RwLock, RwLockReadGuard};


fn other_io_err<E>(e: E) -> io::Error where E: ToString {
	io::Error::new(io::ErrorKind::Other, e.to_string())
}

/// LMDB-backed database.
#[derive(Debug)]
pub struct Database {
	columns: u32,
	path: String,
	// write lock only on db.restore
	env: RwLock<Option<EnvironmentWithDatabases>>,
}

// Duplicate declaration of methods here to avoid trait import in certain existing cases
// at time of addition.
impl Database {
	/// Opens the database path. Creates if it does not exist.
	/// `columns` is a number of non-default columns.
	pub fn open(path: &str, columns: u32) -> io::Result<Self> {
		Ok(Self {
			columns,
			path: path.to_owned(),
			env: RwLock::new(Some(EnvironmentWithDatabases::open(path, columns)?)),
		})
	}

	pub fn get(&self, col: Option<u32>, key: &[u8]) -> io::Result<Option<DBValue>> {
		match *self.env.read() {
			Some(ref env) => env.get(col, key),
			None => Ok(None),
		}
	}

	pub fn write_buffered(&self, txn: DBTransaction) {
		match *self.env.read() {
			Some(ref env) => env.write_buffered(txn),
			_ => (),
		}
	}

	pub fn write(&self, transaction: DBTransaction) -> io::Result<()> {
		match *self.env.read() {
			Some(ref env) => env.write(transaction),
			None => Err(other_io_err("Database is closed")),
		}
	}

	pub fn flush(&self) -> io::Result<()> {
		match *self.env.read() {
			Some(ref env) => env.flush(),
			None => Err(other_io_err("Database is closed")),
		}
	}

	pub fn iter<'env>(
		&'env self,
		col: Option<u32>,
	) -> impl Iterator<Item=(Box<[u8]>, Box<[u8]>)> + 'env {
		IterWithTxnAndRwlock {
			inner: OwningHandle::new_with_fn(
				Box::new(self.env.read()),
				move |rwlock| {
					let rwlock = unsafe { rwlock.as_ref().expect("can't be null; qed") };
					Box::new(rwlock.as_ref().and_then(|env| env.iter(col)))
				}
			),
		}
	}

	pub fn get_by_prefix(&self, col: Option<u32>, prefix: &[u8]) -> Option<Box<[u8]>> {
		match self.iter_from_prefix(col, prefix).next() {
			Some((k, v)) => if k.starts_with(prefix) { Some(v) } else { None },
			_ => None,
		}
	}

	pub fn iter_from_prefix<'env>(
		&'env self,
		col: Option<u32>,
		prefix: &[u8],
	) -> impl Iterator<Item=(Box<[u8]>, Box<[u8]>)> + 'env {
		IterWithTxnAndRwlock {
			inner: OwningHandle::new_with_fn(
				Box::new(self.env.read()),
				move |rwlock| {
					let rwlock = unsafe { rwlock.as_ref().expect("can't be null; qed") };
					Box::new(rwlock.as_ref().and_then(|env| env.iter_from_prefix(col, prefix)))
				}
			),
		}
	}

	/// Close the database
	fn close(&self) {
		*self.env.write() = None;
	}

	/// Restore the database from a copy at given path.
	// TODO: reuse code with rocksdb
	pub fn restore(&self, new_db: &str) -> io::Result<()> {
		self.close();

		// swap is guaranteed to be atomic
		match swap(new_db, &self.path) {
			Ok(_) => {
				// ignore errors
				let _ = fs::remove_dir_all(new_db);
			},
			Err(err) => {
				debug!("DB atomic swap failed: {}", err);
				match swap_nonatomic(new_db, &self.path) {
					Ok(_) => {
						// ignore errors
						let _ = fs::remove_dir_all(new_db);
					},
					Err(err) => {
						warn!("Failed to swap DB directories: {:?}", err);
						return Err(io::Error::new(io::ErrorKind::Other, "DB restoration failed: could not swap DB directories"));
					}
				}
			}
		}

		// reopen the database and steal handles into self
		let db = Self::open(&self.path, self.columns)?;
		*self.env.write() = mem::replace(&mut *db.env.write(), None);
		Ok(())
	}
}

/// An LMDB `Environment` is a collection of one or more DBs,
/// along with transactions and iterators.
#[derive(Debug)]
struct EnvironmentWithDatabases {
	// Transactions are atomic across all DBs in an `Environment`.
	env: Environment,
	// We use one DB per column.
	// `LmdbDatabase` is essentially a `c_int` (a `Copy` type).
	dbs: Vec<LmdbDatabase>,
}

fn open_or_create_db(env: &Environment, col: u32) -> io::Result<LmdbDatabase> {
	let db_name = format!("col{}", col);
	env.create_db(Some(&db_name[..]), DatabaseFlags::default()).map_err(other_io_err)
}

impl EnvironmentWithDatabases {
	fn open(path: &str, columns: u32) -> io::Result<Self> {
		// account for the default column
		let columns = columns + 1;
		let path = Path::new(path);

		let mut env_builder = Environment::new();
		env_builder.set_max_dbs(columns);
		// TODO: this would fail on 32-bit systems
		// double when full? cf. https://github.com/BVLC/caffe/pull/3731
		// TODO: is memory budgeting possible?
		let terabyte: usize = 1 << 40;
		env_builder.set_map_size(terabyte);

		let env = env_builder.open(&path).map_err(other_io_err)?;

		let mut dbs = Vec::with_capacity(columns as usize);
		for col in 0..columns {
			let db = open_or_create_db(&env, col)?;
			dbs.push(db);
		}

		Ok(Self {
			env,
			dbs,
		})
	}

	fn ro_txn(&self) -> io::Result<RoTransaction> {
		self.env.begin_ro_txn().map_err(other_io_err)
	}

	fn rw_txn(&self) -> io::Result<RwTransaction> {
		self.env.begin_rw_txn().map_err(other_io_err)
	}

	fn column_to_db(&self, col: Option<u32>) -> LmdbDatabase {
		let col = col.map_or(0, |c| (c as usize + 1));
		self.dbs[col]
	}

	fn get(&self, col: Option<u32>, key: &[u8]) -> io::Result<Option<DBValue>> {
		let ro_txn = self.ro_txn()?;
		let db = self.column_to_db(col);

		let result = ro_txn.get(db, &key)
			.map(DBValue::from_slice);

		match result {
			Ok(value) => Ok(Some(value)),
			Err(Error::NotFound) => Ok(None),
			Err(e) => Err(other_io_err(e)),
		}
	}

	fn write_buffered(&self, txn: DBTransaction) {
		// TODO: this method actually flushes the data to disk.
		//       Shall we use `NO_SYNC` (doesn't flush, but a system crash can corrupt the database)?
		if let Err(e) = self.write(txn) {
			warn!(target: "lmdb", "error while writing a transaction {:?}", e);
		}
	}

	fn write(&self, transaction: DBTransaction) -> io::Result<()> {
		let mut rw_txn = self.rw_txn()?;

		for op in transaction.ops {
			match op {
				DBOp::Insert { col, key, value } => {
					debug_assert!(key.len() < 512, "lmdb: MDB_MAXKEYSIZE is 511");
					let db = self.column_to_db(col);
					rw_txn.put(db, &key, &value, WriteFlags::empty()).map_err(other_io_err)?;
				},
				DBOp::Delete { col, key } => {
					let db = self.column_to_db(col);
					rw_txn.del(db, &key, None).map_err(other_io_err)?;
				}
			}
		}

		rw_txn.commit().map_err(other_io_err)
	}

	fn flush(&self) -> io::Result<()> {
		// TODO: this only make sense for `NO_SYNC`.
		// self.env.sync(true).map_err(other_io_err)
		self.env.sync(false).map_err(other_io_err)
	}

	fn iter<'env>(&'env self, col: Option<u32>) -> Option<IterWithTxn> {
		// TODO: how to handle errors properly?
		let ro_txn = self.ro_txn().ok()?;
		let db = self.column_to_db(col);

		Some(IterWithTxn {
			inner: OwningHandle::new_with_fn(
				Box::new(ro_txn),
				move |txn| {
					let txn = unsafe { txn.as_ref().expect("can't be null; qed") };
					let mut cursor = txn.open_ro_cursor(db).expect("lmdb: failed to open a cursor");
					let iter = cursor.iter();
					Box::new(Iter {
						iter,
						cursor,
					})
				}
			),
		})
	}

	fn iter_from_prefix(&self, col: Option<u32>, prefix: &[u8]) -> Option<IterWithTxn> {
		let ro_txn = self.ro_txn().ok()?;
		let db = self.column_to_db(col);

		Some(IterWithTxn {
			inner: OwningHandle::new_with_fn(
				Box::new(ro_txn),
				move |txn| {
					let txn = unsafe { txn.as_ref().expect("can't be null; qed") };
					let mut cursor = txn.open_ro_cursor(db).expect("lmdb: failed to open a cursor");
					let iter = cursor.iter_from(prefix);
					Box::new(Iter {
						iter,
						cursor,
					})
				}
			),
		})
	}
}

impl Drop for EnvironmentWithDatabases {
	fn drop(&mut self) {
		if let Err(error) = self.flush() {
			warn!(target: "lmdb", "database flush failed: {}", error);
		}
	}
}

struct Iter<'env> {
    iter: LmdbIter<'env>,
    cursor: RoCursor<'env>,
}

impl<'env> Iterator for Iter<'env> {
    type Item = Result<(&'env [u8], &'env [u8]), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next()
    }
}

impl<'env> Drop for Iter<'env> {
	fn drop(&mut self) {
		drop(&mut self.cursor);
	}
}

// TODO: is there a better way to implement an iterator?
// If we return just Iter, the brrwchk complains (because of ro_txn lifetime, rightly so).
// I'm open to any suggestions.
// TODO: avoid Boxes via a wrapper, which implements Deref and StableDeref.
struct IterWithTxn<'env> {
	inner: OwningHandle<Box<RoTransaction<'env>>, Box<Iter<'env>>>,
}

// oh boy
struct IterWithTxnAndRwlock<'env> {
	inner: OwningHandle<
		Box<RwLockReadGuard<'env, Option<EnvironmentWithDatabases>>>,
		Box<Option<IterWithTxn<'env>>>,
	>,
}

impl<'env> Iterator for IterWithTxn<'env> {
	type Item = (Box<[u8]>, Box<[u8]>);

	fn next(&mut self) -> Option<Self::Item> {
		// TODO: panic instead of silencing errors?
		match self.inner.deref_mut().next().and_then(Result::ok) {
			Some((key, value)) => {
				Some((
					key.to_vec().into_boxed_slice(),
					value.to_vec().into_boxed_slice(),
				))
			},
			_ => None,
		}
	}
}

impl<'env> Iterator for IterWithTxnAndRwlock<'env> {
	type Item = (Box<[u8]>, Box<[u8]>);

	fn next(&mut self) -> Option<Self::Item> {
		self.inner
			.deref_mut()
			.as_mut()
			.and_then(|iter| iter.next())
	}
}

impl KeyValueDB for Database {
	fn get(&self, col: Option<u32>, key: &[u8]) -> io::Result<Option<DBValue>> {
		Database::get(self, col, key)
	}

	fn get_by_prefix(&self, col: Option<u32>, prefix: &[u8]) -> Option<Box<[u8]>> {
		Database::get_by_prefix(self, col, prefix)
	}

	fn write_buffered(&self, transaction: DBTransaction) {
		Database::write_buffered(self, transaction)
	}

	fn write(&self, transaction: DBTransaction) -> io::Result<()> {
		Database::write(self, transaction)
	}

	fn flush(&self) -> io::Result<()> {
		Database::flush(self)
	}

	fn iter<'a>(&'a self, col: Option<u32>) -> Box<Iterator<Item=(Box<[u8]>, Box<[u8]>)> + 'a> {
		let unboxed = Database::iter(self, col);
		Box::new(unboxed)
	}

	fn iter_from_prefix<'a>(&'a self, col: Option<u32>, prefix: &'a [u8])
		-> Box<Iterator<Item=(Box<[u8]>, Box<[u8]>)> + 'a>
	{
		let unboxed = Database::iter_from_prefix(self, col, prefix);
		Box::new(unboxed)
	}

	fn restore(&self, new_db: &str) -> io::Result<()> {
		Database::restore(self, new_db)
	}
}