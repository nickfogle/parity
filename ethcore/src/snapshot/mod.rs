// Copyright 2015, 2016 Ethcore (UK) Ltd.
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

//! Snapshot creation helpers.

use std::collections::VecDeque;
use std::fs::{create_dir_all, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use account_db::{AccountDB, AccountDBMut};
use client::BlockChainClient;
use error::Error;
use ids::BlockID;
use views::{BlockView, HeaderView};

use util::{Bytes, Hashable, HashDB, JournalDB, snappy, TrieDB, TrieDBMut, TrieMut, DBTransaction};
use util::error::UtilError;
use util::hash::{FixedHash, H256};
use util::rlp::{DecoderError, RlpStream, Stream, UntrustedRlp, View, Compressible, RlpType};

use self::account::Account;
use self::block::AbridgedBlock;

use crossbeam::{scope, ScopedJoinHandle};

mod account;
mod block;

// Try to have chunks be around 16MB (before compression)
const PREFERRED_CHUNK_SIZE: usize = 16 * 1024 * 1024;

/// Take a snapshot using the given client and database, writing into `path`.
pub fn take_snapshot(client: &BlockChainClient, mut path: PathBuf, state_db: &HashDB) -> Result<(), Error> {
	let chain_info = client.chain_info();

	let genesis_hash = chain_info.genesis_hash;
	let best_header_raw = client.best_block_header();
	let best_header = HeaderView::new(&best_header_raw);
	let state_root = best_header.state_root();

	trace!(target: "snapshot", "Taking snapshot starting at block {}", best_header.number());

	let _ = create_dir_all(&path);

	let state_hashes = try!(chunk_state(state_db, &state_root, &path));
	let block_hashes = try!(chunk_blocks(client, best_header.hash(), genesis_hash, &path));

	trace!(target: "snapshot", "produced {} state chunks and {} block chunks.", state_hashes.len(), block_hashes.len());

	let manifest_data = ManifestData {
		state_hashes: state_hashes,
		block_hashes: block_hashes,
		state_root: state_root,
		block_number: chain_info.best_block_number,
		block_hash: chain_info.best_block_hash,
	};

	path.push("MANIFEST");

	let mut manifest_file = try!(File::create(&path));

	try!(manifest_file.write_all(&manifest_data.into_rlp()));

	Ok(())
}

// shared portion of write_chunk
// returns either a (hash, compressed_size) pair or an io error.
fn write_chunk(raw_data: &[u8], compression_buffer: &mut Vec<u8>, path: &Path) -> Result<(H256, usize), Error> {
	let compressed_size = snappy::compress_into(raw_data, compression_buffer);
	let compressed = &compression_buffer[..compressed_size];
	let hash = compressed.sha3();

	let mut file_path = path.to_owned();
	file_path.push(hash.hex());

	let mut file = try!(File::create(file_path));
	try!(file.write_all(compressed));

	Ok((hash, compressed_size))
}

/// Used to build block chunks.
struct BlockChunker<'a> {
	client: &'a BlockChainClient,
	// block, receipt rlp pairs.
	rlps: VecDeque<Bytes>,
	current_hash: H256,
	hashes: Vec<H256>,
	snappy_buffer: Vec<u8>,
}

impl<'a> BlockChunker<'a> {
	// Repeatedly fill the buffers and writes out chunks, moving backwards from starting block hash.
	// Loops until we reach the genesis, and writes out the remainder.
	fn chunk_all(&mut self, genesis_hash: H256, path: &Path) -> Result<(), Error> {
		let mut loaded_size = 0;

		while self.current_hash != genesis_hash {
			let block = self.client.block(BlockID::Hash(self.current_hash))
				.expect("started from the head of chain and walking backwards; client stores full chain; qed");
			let view = BlockView::new(&block);
			let abridged_rlp = AbridgedBlock::from_block_view(&view).into_inner();

			let receipts = self.client.block_receipts(&self.current_hash)
				.expect("started from head of chain and walking backwards; client stores full chain; qed");

			let pair = {
				let mut pair_stream = RlpStream::new_list(2);
				pair_stream.append(&abridged_rlp).append(&receipts);
				pair_stream.out()
			};

			let new_loaded_size = loaded_size + pair.len();

			// cut off the chunk if too large
			if new_loaded_size > PREFERRED_CHUNK_SIZE {
				let header = view.header_view();
				try!(self.write_chunk(header.parent_hash(), header.number(), path));
				loaded_size = pair.len();
			} else {
				loaded_size = new_loaded_size;
			}

			self.rlps.push_front(pair);
			self.current_hash = view.header_view().parent_hash();
		}

		if loaded_size != 0 {
			// we don't store the genesis block, so once we get to this point,
			// the "first" block will be number 1.
			try!(self.write_chunk(genesis_hash, 1, path));
		}

		Ok(())
	}

	// write out the data in the buffers to a chunk on disk
	fn write_chunk(&mut self, parent_hash: H256, number: u64, path: &Path) -> Result<(), Error> {
		trace!(target: "snapshot", "prepared block chunk with {} blocks", self.rlps.len());
		let mut rlp_stream = RlpStream::new_list(self.rlps.len() + 2);
		rlp_stream.append(&parent_hash).append(&number);
		for pair in self.rlps.drain(..) {
			rlp_stream.append_raw(&pair, 1);
		}

		let raw_data = rlp_stream.out();
		let (hash, size) = try!(write_chunk(&raw_data, &mut self.snappy_buffer, path));
		trace!(target: "snapshot", "wrote block chunk. hash: {}, size: {}, uncompressed size: {}", hash.hex(), size, raw_data.len());

		self.hashes.push(hash);
		Ok(())
	}
}

/// Create and write out all block chunks to disk, returning a vector of all
/// the hashes of block chunks created.
///
/// The path parameter is the directory to store the block chunks in.
/// This function assumes the directory exists already.
pub fn chunk_blocks(client: &BlockChainClient, best_block_hash: H256, genesis_hash: H256, path: &Path) -> Result<Vec<H256>, Error> {
	let mut chunker = BlockChunker {
		client: client,
		rlps: VecDeque::new(),
		current_hash: best_block_hash,
		hashes: Vec::new(),
		snappy_buffer: vec![0; snappy::max_compressed_len(PREFERRED_CHUNK_SIZE)],
	};

	try!(chunker.chunk_all(genesis_hash, path));

	Ok(chunker.hashes)
}

/// State trie chunker.
struct StateChunker<'a> {
	hashes: Vec<H256>,
	rlps: Vec<Bytes>,
	cur_size: usize,
	snapshot_path: &'a Path,
	snappy_buffer: Vec<u8>,
}

impl<'a> StateChunker<'a> {
	// Push a key, value pair to be encoded.
	//
	// If the buffer is greater than the desired chunk size,
	// this will write out the data to disk.
	fn push(&mut self, account_hash: Bytes, data: Bytes) -> Result<(), Error> {
		let pair = {
			let mut stream = RlpStream::new_list(2);
			stream.append(&account_hash).append_raw(&data, 1);
			stream.out()
		};

		if self.cur_size + pair.len() >= PREFERRED_CHUNK_SIZE {
			try!(self.write_chunk());
		}

		self.cur_size += pair.len();
		self.rlps.push(pair);

		Ok(())
	}

	// Write out the buffer to disk, pushing the created chunk's hash to
	// the list.
	fn write_chunk(&mut self) -> Result<(), Error> {
		let mut stream = RlpStream::new_list(self.rlps.len());
		for rlp in self.rlps.drain(..) {
			stream.append_raw(&rlp, 1);
		}

		let raw_data = stream.out();
		let (hash, compressed_size) = try!(write_chunk(&raw_data, &mut self.snappy_buffer, self.snapshot_path));
		trace!(target: "snapshot", "wrote state chunk. size: {}, uncompressed size: {}", compressed_size, raw_data.len());

		self.hashes.push(hash);
		self.cur_size = 0;

		Ok(())
	}
}

/// Walk the given state database starting from the given root,
/// creating chunks and writing them out.
///
/// Returns a list of hashes of chunks created, or any error it may
/// have encountered.
pub fn chunk_state(db: &HashDB, root: &H256, path: &Path) -> Result<Vec<H256>, Error> {
	let account_view = try!(TrieDB::new(db, &root));

	let mut chunker = StateChunker {
		hashes: Vec::new(),
		rlps: Vec::new(),
		cur_size: 0,
		snapshot_path: path,
		snappy_buffer: vec![0; snappy::max_compressed_len(PREFERRED_CHUNK_SIZE)],
	};

	trace!(target: "snapshot", "beginning state chunking");

	// account_key here is the address' hash.
	for (account_key, account_data) in account_view.iter() {
		let account = Account::from_thin_rlp(account_data);
		let account_key_hash = H256::from_slice(&account_key);

		let account_db = AccountDB::from_hash(db, account_key_hash);

		let fat_rlp = try!(account.to_fat_rlp(&account_db));
		let compressed_rlp = UntrustedRlp::new(&fat_rlp).compress(RlpType::Snapshot).to_vec();
		try!(chunker.push(account_key, compressed_rlp));
	}

	if chunker.cur_size != 0 {
		try!(chunker.write_chunk());
	}

	Ok(chunker.hashes)
}

/// Manifest data.
pub struct ManifestData {
	/// List of state chunk hashes.
	pub state_hashes: Vec<H256>,
	/// List of block chunk hashes.
	pub block_hashes: Vec<H256>,
	/// The final, expected state root.
	pub state_root: H256,
	/// Block number this snapshot was taken at.
	pub block_number: u64,
	/// Block hash this snapshot was taken at.
	pub block_hash: H256,
}

impl ManifestData {
	/// Encode the manifest data to rlp.
	pub fn into_rlp(self) -> Bytes {
		let mut stream = RlpStream::new_list(5);
		stream.append(&self.state_hashes);
		stream.append(&self.block_hashes);
		stream.append(&self.state_root);
		stream.append(&self.block_number);
		stream.append(&self.block_hash);

		stream.out()
	}

	/// Try to restore manifest data from raw bytes, interpreted as RLP.
	pub fn from_rlp(raw: &[u8]) -> Result<Self, DecoderError> {
		let decoder = UntrustedRlp::new(raw);

		let state_hashes: Vec<H256> = try!(decoder.val_at(0));
		let block_hashes: Vec<H256> = try!(decoder.val_at(1));
		let state_root: H256 = try!(decoder.val_at(2));
		let block_number: u64 = try!(decoder.val_at(3));
		let block_hash: H256 = try!(decoder.val_at(4));

		Ok(ManifestData {
			state_hashes: state_hashes,
			block_hashes: block_hashes,
			state_root: state_root,
			block_number: block_number,
			block_hash: block_hash,
		})
	}
}

/// Used to rebuild the state trie piece by piece.
pub struct StateRebuilder {
	db: Box<JournalDB>,
	state_root: H256,
	snappy_buffer: Vec<u8>
}

impl StateRebuilder {
	/// Create a new state rebuilder to write into the given backing DB.
	pub fn new(db: Box<JournalDB>) -> Self {
		StateRebuilder {
			db: db,
			state_root: H256::zero(),
			snappy_buffer: Vec::new(),
		}
	}

	/// Feed a compressed state chunk into the rebuilder.
	pub fn feed(&mut self, compressed: &[u8]) -> Result<(), Error> {
		let len = try!(snappy::decompress_into(compressed, &mut self.snappy_buffer));
		let rlp = UntrustedRlp::new(&self.snappy_buffer[..len]);
		let account_fat_rlps: Vec<_> = rlp.iter().map(|r| r.as_raw()).collect();
		let mut pairs = Vec::with_capacity(rlp.item_count());

		// initialize the pairs vector with empty values so we have slots to write into.
		for _ in 0..rlp.item_count() {
			pairs.push((H256::new(), Vec::new()));
		}

		let chunk_size = account_fat_rlps.len() / ::num_cpus::get();

		// build account tries in parallel.
		try!(scope(|scope| {
			let mut handles = Vec::new();
			for (account_chunk, out_pairs_chunk) in account_fat_rlps.chunks(chunk_size).zip(pairs.chunks_mut(chunk_size)) {
				let mut db = self.db.boxed_clone();
				let handle: ScopedJoinHandle<Result<(), Error>> = scope.spawn(move || {
					try!(rebuild_account_trie(db.as_hashdb_mut(), account_chunk, out_pairs_chunk));

					// commit the db changes we made in this thread.
					let batch = DBTransaction::new(&db.backing());
					try!(db.commit(&batch, 0, &H256::zero(), None));
					try!(db.backing().write(batch).map_err(UtilError::SimpleString));

					Ok(())
				});

				handles.push(handle);
			}

			// see if we got any errors.
			for handle in handles {
				try!(handle.join());
			}

			Ok::<_, Error>(())
		}));

		// batch trie writes
		{
			let mut account_trie = if self.state_root != H256::zero() {
				try!(TrieDBMut::from_existing(self.db.as_hashdb_mut(), &mut self.state_root))
			} else {
				TrieDBMut::new(self.db.as_hashdb_mut(), &mut self.state_root)
			};

			for (hash, thin_rlp) in pairs {
				try!(account_trie.insert(&hash, &thin_rlp));
			}
		}

		let batch = DBTransaction::new(self.db.backing());
		try!(self.db.commit(&batch, 0, &H256::zero(), None));
		try!(self.db.backing().write(batch).map_err(|e| Error::Util(e.into())));
		Ok(())
	}

	/// Get the state root of the rebuilder.
	pub fn state_root(&self) -> H256 { self.state_root }
}

fn rebuild_account_trie(db: &mut HashDB, account_chunk: &[&[u8]], out_chunk: &mut [(H256, Bytes)]) -> Result<(), Error> {
	for (account_pair, out) in account_chunk.into_iter().zip(out_chunk) {
		let account_rlp = UntrustedRlp::new(account_pair);

		let hash: H256 = try!(account_rlp.val_at(0));
		let decompressed = try!(account_rlp.at(1)).decompress(RlpType::Snapshot);
		let fat_rlp = UntrustedRlp::new(&decompressed[..]);

		let thin_rlp = {
			let mut acct_db = AccountDBMut::from_hash(db.as_hashdb_mut(), hash);

			// fill out the storage trie and code while decoding.
			let acc = try!(Account::from_fat_rlp(&mut acct_db, fat_rlp));

			acc.to_thin_rlp()
		};

		*out = (hash, thin_rlp);
	}
	Ok(())
}
