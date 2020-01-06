// Copyright 2015-2019 Parity Technologies (UK) Ltd.
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

//! Key-Value store abstraction with `RocksDB` backend.

use bytes::Bytes;
use smallvec::SmallVec;
use std::io;
use std::path::Path;
use std::sync::Arc;

/// Required length of prefixes.
pub const PREFIX_LEN: usize = 12;

/// Database value.
pub type DBValue = Vec<u8>;
/// Database keys.
pub type DBKey = SmallVec<[u8; 32]>;

#[derive(Clone, PartialEq)]
enum DBOpIndex {
	Insert { col: u32, key_len: usize, value_len: usize },
	Delete { col: u32, key_len: usize },
}

#[derive(Default, Clone, PartialEq)]
pub struct DBTransaction {
	ops: Vec<u8>,
	index: Vec<DBOpIndex>,
}

impl DBTransaction {
	pub fn new() -> Self {
		Self::with_capacity(256)
	}

	/// Create new transaction with capacity.
	pub fn with_capacity(cap: usize) -> Self {
		Self {
			ops: Vec::with_capacity(cap << 6),
			index: Vec::with_capacity(cap),
		}
	}

	/// Insert a key-value pair in the transaction. Any existing value will be overwritten upon write.
	pub fn put(&mut self, col: u32, key: &[u8], value: &[u8]) {
		self.ops.extend_from_slice(key);
		self.ops.extend_from_slice(value);
		self.index.push(DBOpIndex::Insert { col: col, key_len: key.len(), value_len: value.len() });
	}

	/// Insert a key-value pair in the transaction. Any existing value will be overwritten upon write.
	pub fn put_vec(&mut self, col: u32, key: &[u8], value: Bytes) {
		self.put(col, key, &value[..]);
	}

	/// Delete value by key.
	pub fn delete(&mut self, col: u32, key: &[u8]) {
		self.ops.extend_from_slice(key);
		self.index.push(DBOpIndex::Delete { col: col, key_len: key.len() });
	}

	pub fn iter(&self) -> DataOpIterator {
		DataOpIterator { tx: self, pos: 0, index_pos: 0  }
	}
}

pub enum DataOp<'a> {
	Insert {
		col: u32,
		key: &'a [u8],
		val: &'a [u8],
	},
	Delete {
		col: u32,
		key: &'a [u8]
	},
}

impl DataOp<'_> {
	pub fn col(&self) -> u32 {
		match &self {
			DataOp::Insert { col, .. } => *col,
			DataOp::Delete { col, .. } => *col,
		}
	}
}

pub struct DataOpIterator<'a> {
	tx: &'a DBTransaction,
	pos: usize,
	index_pos: usize,
}

impl<'a> Iterator for DataOpIterator<'a> {
	type Item = DataOp<'a>;

	fn next(&mut self) -> Option<Self::Item> {
		if self.index_pos == self.tx.index.len() { return None; }

		Some(match &self.tx.index[self.index_pos] {
			DBOpIndex::Insert { col, key_len, value_len } => {
				let ret = DataOp::Insert {
					col: *col,
					key: &self.tx.ops[self.pos..self.pos + key_len],
					val: &self.tx.ops[self.pos+key_len..self.pos+key_len+value_len],
				};
				self.pos += key_len + value_len;
				self.index_pos += 1;
				ret
			}
			DBOpIndex::Delete { col, key_len } => {
				let ret = DataOp::Delete {
					col: *col,
					key: &self.tx.ops[self.pos..self.pos + key_len ],
				};
				self.pos += key_len;
				self.index_pos += 1;
				ret
			}
		})
	}
}

/// Database operation.
#[derive(Clone, PartialEq)]
pub enum DBOp {
	Insert { col: u32, key: DBKey, value: DBValue },
	Delete { col: u32, key: DBKey },
}

impl DBOp {
	/// Returns the key associated with this operation.
	pub fn key(&self) -> &[u8] {
		match *self {
			DBOp::Insert { ref key, .. } => key,
			DBOp::Delete { ref key, .. } => key,
		}
	}

	/// Returns the column associated with this operation.
	pub fn col(&self) -> u32 {
		match *self {
			DBOp::Insert { col, .. } => col,
			DBOp::Delete { col, .. } => col,
		}
	}
}

/// Generic key-value database.
///
/// This makes a distinction between "buffered" and "flushed" values. Values which have been
/// written can always be read, but may be present in an in-memory buffer. Values which have
/// been flushed have been moved to backing storage, like a RocksDB instance. There are certain
/// operations which are only guaranteed to operate on flushed data and not buffered,
/// although implementations may differ in this regard.
///
/// The contents of an interior buffer may be explicitly flushed using the `flush` method.
///
/// The `KeyValueDB` also deals in "column families", which can be thought of as distinct
/// stores within a database. Keys written in one column family will not be accessible from
/// any other. The number of column families must be specified at initialization, with a
/// differing interface for each database. The `None` argument in place of a column index
/// is always supported.
///
/// The API laid out here, along with the `Sync` bound implies interior synchronization for
/// implementation.
pub trait KeyValueDB: Sync + Send + parity_util_mem::MallocSizeOf {
	/// Helper to create new transaction.
	fn transaction(&self) -> DBTransaction {
		DBTransaction::new()
	}

	/// Get a value by key.
	fn get(&self, col: u32, key: &[u8]) -> io::Result<Option<DBValue>>;

	/// Get a value by partial key. Only works for flushed data.
	fn get_by_prefix(&self, col: u32, prefix: &[u8]) -> Option<Box<[u8]>>;

	/// Write a transaction of changes to the buffer.
	fn write_buffered(&self, transaction: DBTransaction);

	/// Write a transaction of changes to the backing store.
	fn write(&self, transaction: DBTransaction) -> io::Result<()> {
		self.write_buffered(transaction);
		self.flush()
	}

	/// Flush all buffered data.
	fn flush(&self) -> io::Result<()>;

	/// Iterate over flushed data for a given column.
	fn iter<'a>(&'a self, col: u32) -> Box<dyn Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a>;

	/// Iterate over flushed data for a given column, starting from a given prefix.
	fn iter_from_prefix<'a>(
		&'a self,
		col: u32,
		prefix: &'a [u8],
	) -> Box<dyn Iterator<Item = (Box<[u8]>, Box<[u8]>)> + 'a>;

	/// Attempt to replace this database with a new one located at the given path.
	fn restore(&self, new_db: &str) -> io::Result<()>;
}
