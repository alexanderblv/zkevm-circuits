//! Implementation of an in-memory key-value database to represent the
//! Ethereum State Trie.

use crate::{
    utils::{hash_code, is_precompiled},
    Address, Hash, Word, H256, KECCAK_CODE_HASH_EMPTY, U256,
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::LazyLock,
};

static ACCOUNT_ZERO: LazyLock<Account> = LazyLock::new(Account::zero);
/// Hash value for empty code hash.
static EMPTY_CODE_HASH: LazyLock<Hash> = LazyLock::new(|| CodeDB::hash(&[]));
/// bytes of empty code hash, in little endian order.
pub static EMPTY_CODE_HASH_LE: LazyLock<[u8; 32]> = LazyLock::new(|| {
    let mut bytes = EMPTY_CODE_HASH.to_fixed_bytes();
    bytes.reverse();
    bytes
});

const VALUE_ZERO: Word = Word::zero();

/// Memory storage for contract code by code hash.
#[derive(Debug)]
pub struct CodeDB(pub HashMap<Hash, Vec<u8>>);

impl Clone for CodeDB {
    fn clone(&self) -> Self {
        CodeDB(self.0.clone())
    }
}

impl Default for CodeDB {
    fn default() -> Self {
        Self::new()
    }
}

impl CodeDB {
    /// Create a new empty Self.
    pub fn new() -> Self {
        let mut codedb = Self(HashMap::new());
        codedb.insert(Vec::new());
        codedb
    }
    /// Insert code along with code hash
    pub fn insert_with_hash(&mut self, hash: H256, code: Vec<u8>) {
        self.0.insert(hash, code);
    }
    /// Insert code to CodeDB, and return the code hash.
    pub fn insert(&mut self, code: Vec<u8>) -> Hash {
        let hash = Self::hash(&code);
        self.insert_with_hash(hash, code);
        hash
    }
    /// Specify code hash for empty code (nil)
    pub fn empty_code_hash() -> Hash {
        *EMPTY_CODE_HASH
    }

    /// Compute hash of given code.
    pub fn hash(code: &[u8]) -> Hash {
        H256(hash_code(code).into())
    }
}

/// Account of the Ethereum State Trie, which contains an in-memory key-value
/// database that represents the Account Storage Trie.
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct Account {
    /// Nonce
    pub nonce: Word,
    /// Balance
    pub balance: Word,
    /// Storage key-value map
    pub storage: HashMap<Word, Word>,
    /// Poseidon hash of code
    pub code_hash: Hash,
    /// Keccak hash of code
    pub keccak_code_hash: Hash,
    /// Size of code, i.e. code length
    pub code_size: Word,
}

impl Account {
    /// Return an empty account, with all values set at zero.
    pub fn zero() -> Self {
        Self {
            nonce: Word::zero(),
            balance: Word::zero(),
            storage: HashMap::new(),
            code_hash: CodeDB::empty_code_hash(),
            keccak_code_hash: *KECCAK_CODE_HASH_EMPTY,
            code_size: Word::zero(),
        }
    }

    /// Return if account is empty or not.
    pub fn is_empty(&self) -> bool {
        debug_assert_ne!(
            self.code_hash,
            Hash::zero(),
            "codehash inside statedb should never be 0, {self:?}"
        );
        let is_code_hash_empty = self.code_hash.eq(&CodeDB::empty_code_hash());
        if is_code_hash_empty {
            debug_assert_eq!(Word::zero(), self.code_size);
        }
        self.nonce.is_zero() && self.balance.is_zero() && is_code_hash_empty
    }

    /// Return the expected read code hash, i.e. in
    /// empty (non-existed) account it is expected
    /// to be 0
    pub fn code_hash_read(&self) -> Hash {
        if self.is_empty() {
            Hash::zero()
        } else {
            self.code_hash
        }
    }
}

/// In-memory key-value database that represents the Ethereum State Trie.
#[derive(Debug, Clone, Default)]
pub struct StateDB {
    state: HashMap<Address, Account>,

    // Fields with transaction lifespan, will be clear in `clear_access_list_and_refund`.
    access_list_account: HashSet<Address>,
    access_list_account_storage: HashSet<(Address, U256)>,
    // `dirty_storage` contains writes during current transaction.
    // When current transaction finishes, `dirty_storage` will be committed into `state`.
    // The reason why we need this is that EVM needs committed state, namely
    // state before current transaction, to calculate gas cost for some opcodes like sstore.
    // So both dirty storage and committed storage are needed.
    dirty_storage: HashMap<(Address, Word), Word>,
    // Transient storage, which is cleared after the transaction.
    transient_storage: HashMap<(Address, Word), Word>,
    // Accounts that have been through `SELFDESTRUCT` under the situation that `is_persistent` is
    // `true`. These accounts will be reset once `commit_tx` is called.
    destructed_account: HashSet<Address>,
    // Accounts that are still "empty", but an Account Rw {value_prev: 0x0, value: empty_code_hash}
    // has already been applied.
    // TODO: a better name?
    touched_account: HashSet<Address>,
    refund: u64,
}

impl StateDB {
    /// Create an empty Self
    pub fn new() -> Self {
        Self::default()
    }

    /// Set an [`Account`] at `addr` in the StateDB.
    pub fn set_account(&mut self, addr: &Address, acc: Account) {
        self.state.insert(*addr, acc);
    }

    /// Get a reference to the [`Account`] at `addr`.  Returns false and a zero
    /// [`Account`] when the [`Account`] wasn't found in the state.
    pub fn get_account(&self, addr: &Address) -> (bool, &Account) {
        match self.state.get(addr) {
            Some(acc) => (true, acc),
            None => (false, &(*ACCOUNT_ZERO)),
        }
    }

    /// List all account addresses in current state db
    pub fn list_accounts(&self) {
        let addrs: BTreeSet<_> = self.state.keys().collect();
        log::debug!("sdb list_accounts begin");
        for addr in addrs {
            log::debug!("{addr:?}");
        }
        log::debug!("sdb list_accounts end");
    }

    /// If the returned value is false, then this address is real non existed address.
    /// Any non codehash WriteRw cannot be applied.
    pub fn is_touched(&self, addr: &Address) -> bool {
        self.touched_account.contains(addr)
    }

    /// Even though this addr is still empty, an Account Rw {value_prev: 0x0, value:
    /// empty_code_hash}
    // has already been applied. So furthur Account Write Rw is allowed.
    pub fn set_touched(&mut self, addr: &Address) -> bool {
        self.touched_account.insert(*addr)
    }

    /// Get a mutable reference to the [`Account`] at `addr`.  If the
    /// [`Account`] is not found in the state, a zero one will be inserted
    /// and returned along with false.
    pub fn get_account_mut(&mut self, addr: &Address) -> (bool, &mut Account) {
        let found = if self.state.contains_key(addr) {
            true
        } else {
            log::trace!("insert empty account for addr {:?}", addr);
            self.state.insert(*addr, Account::zero());
            false
        };
        (found, self.state.get_mut(addr).expect("addr not inserted"))
    }

    /// Get a reference to the storage value from [`Account`] at `addr`, at
    /// `key`.  Returns false and a zero [`Word`] when the [`Account`] or `key`
    /// wasn't found in the state.
    /// Returns dirty storage state, which includes writes in current tx
    pub fn get_storage(&self, addr: &Address, key: &Word) -> (bool, &Word) {
        match self.dirty_storage.get(&(*addr, *key)) {
            Some(v) => (true, v),
            None => self.get_committed_storage(addr, key),
        }
    }

    /// Get a reference to the transient storage value from [`Account`] at `addr`, at
    /// `key`.  Returns false and a zero [`Word`] when the [`Account`] or `key`
    /// wasn't found in the state.
    /// Returns transient storage value, which is cleared after current tx
    pub fn get_transient_storage(&self, addr: &Address, key: &Word) -> (bool, &Word) {
        match self.transient_storage.get(&(*addr, *key)) {
            Some(v) => (true, v),
            None => (false, &VALUE_ZERO),
        }
    }

    /// Get a reference to the storage value from [`Account`] at `addr`, at
    /// `key`.  Returns false and a zero [`Word`] when the [`Account`] or `key`
    /// wasn't found in the state.
    /// Returns committed storage, which is storage state before current tx
    pub fn get_committed_storage(&self, addr: &Address, key: &Word) -> (bool, &Word) {
        let (_, acc) = self.get_account(addr);
        match acc.storage.get(key) {
            Some(value) => (true, value),
            None => (false, &VALUE_ZERO),
        }
    }

    /// Get a mutable reference to the storage value from [`Account`] at `addr`,
    /// at `key`.  Returns false when the [`Account`] or `key` wasn't found in
    /// the state and it is created.  If the [`Account`] or `key` is not found
    /// in the state, a zero [`Account`] will be inserted, a zero value will
    /// be inserted at `key` in its storage, and the value will be returned
    /// along with false.
    pub fn get_storage_mut(&mut self, addr: &Address, key: &Word) -> (bool, &mut Word) {
        let (_, acc) = self.get_account_mut(addr);
        let found = if acc.storage.contains_key(key) {
            true
        } else {
            acc.storage.insert(*key, Word::zero());
            false
        };
        (found, acc.storage.get_mut(key).expect("key not inserted"))
    }

    /// Set storage value at `addr` and `key`.
    /// Writes into dirty_storage during transaction execution.
    /// After transaction execution, `dirty_storage` is committed into `storage`
    /// in `commit_tx` method.
    pub fn set_storage(&mut self, addr: &Address, key: &Word, value: &Word) {
        self.dirty_storage.insert((*addr, *key), *value);
    }

    /// Get balance of account with the given address.
    pub fn get_balance(&self, addr: &Address) -> Word {
        let (_, account) = self.get_account(addr);
        account.balance
    }

    /// Set transient storage value at `addr` and `key`.
    /// Transient storage is cleared after transaction execution.
    pub fn set_transient_storage(&mut self, addr: &Address, key: &Word, value: &Word) {
        self.transient_storage.insert((*addr, *key), *value);
    }

    /// Get nonce of account with `addr`.
    pub fn get_nonce(&self, addr: &Address) -> u64 {
        let (_, account) = self.get_account(addr);
        account.nonce.as_u64()
    }

    /// Increase nonce of account with `addr` and return the previous value.
    pub fn increase_nonce(&mut self, addr: &Address) -> u64 {
        let (_, account) = self.get_account_mut(addr);
        let nonce = account.nonce.as_u64();
        account.nonce = account.nonce + 1;
        nonce
    }

    /// Check whether `addr` exists in account access list.
    ///
    /// Note: After the hardfork Berlin,
    /// all the precompiled contracts addresses are always considered warm.
    pub fn check_account_in_access_list(&self, addr: &Address) -> bool {
        is_precompiled(addr) || self.access_list_account.contains(addr)
    }

    /// Add `addr` into account access list. Returns `true` if it's not in the
    /// access list before.
    pub fn add_account_to_access_list(&mut self, addr: Address) -> bool {
        self.access_list_account.insert(addr)
    }

    /// Remove `addr` from account access list.
    pub fn remove_account_from_access_list(&mut self, addr: &Address) {
        let exist = self.access_list_account.remove(addr);
        debug_assert!(exist);
    }

    /// Check whether `(addr, key)` exists in account storage access list.
    pub fn check_account_storage_in_access_list(&self, pair: &(Address, Word)) -> bool {
        self.access_list_account_storage.contains(pair)
    }

    /// Add `(addr, key)` into account storage access list. Returns `true` if
    /// it's not in the access list before.
    pub fn add_account_storage_to_access_list(&mut self, (addr, key): (Address, Word)) -> bool {
        self.access_list_account_storage.insert((addr, key))
    }

    /// Remove `(addr, key)` from account storage access list.
    pub fn remove_account_storage_from_access_list(&mut self, pair: &(Address, Word)) {
        let exist = self.access_list_account_storage.remove(pair);
        debug_assert!(exist);
    }

    /// Set account as self destructed.
    pub fn destruct_account(&mut self, addr: Address) {
        self.state.insert(addr, Account::zero());
        self.destructed_account.insert(addr);
    }

    /// Retrieve refund.
    pub fn refund(&self) -> u64 {
        self.refund
    }

    /// Set refund
    pub fn set_refund(&mut self, value: u64) {
        self.refund = value;
    }

    /// Clear access list and refund, and commit dirty storage.
    /// It should be invoked before processing
    /// with new transaction with the same [`StateDB`].
    pub fn commit_tx(&mut self) {
        self.access_list_account = HashSet::new();
        self.access_list_account_storage = HashSet::new();
        for ((addr, key), value) in self.dirty_storage.clone() {
            let (_, ptr) = self.get_storage_mut(&addr, &key);
            *ptr = value;
        }
        self.dirty_storage = HashMap::new();
        self.touched_account = HashSet::new();
        for addr in self.destructed_account.clone() {
            let (_, account) = self.get_account_mut(&addr);
            *account = ACCOUNT_ZERO.clone();
        }
        self.refund = 0;
    }

    /// Clear transient storage.
    pub fn clear_transient_storage(&mut self) {
        self.transient_storage = HashMap::new();
    }
}

#[cfg(test)]
mod statedb_tests {
    use super::*;
    use crate::address;

    #[test]
    fn statedb() {
        let addr_a = address!("0x0000000000000000000000000000000000000001");
        let addr_b = address!("0x0000000000000000000000000000000000000002");
        let mut statedb = StateDB::new();

        // Get non-existing account
        let (found, acc) = statedb.get_account(&addr_a);
        assert!(!found);
        assert_eq!(acc, &Account::zero());

        // Get non-existing storage key for non-existing account
        let (found, value) = statedb.get_storage(&addr_a, &Word::from(2));
        assert!(!found);
        assert_eq!(value, &Word::zero());

        // Get mut non-existing account and set nonce
        let (found, acc) = statedb.get_account_mut(&addr_a);
        assert!(!found);
        assert_eq!(acc, &Account::zero());
        acc.nonce = Word::from(100);

        // Get existing account and check nonce
        let (found, acc) = statedb.get_account(&addr_a);
        assert!(found);
        assert_eq!(acc.nonce, Word::from(100));

        // Get non-existing storage key for existing account and set value
        let (found, value) = statedb.get_storage_mut(&addr_a, &Word::from(2));
        assert!(!found);
        assert_eq!(value, &Word::zero());
        *value = Word::from(101);

        // Get existing storage key and check value
        let (found, value) = statedb.get_storage(&addr_a, &Word::from(2));
        assert!(found);
        assert_eq!(value, &Word::from(101));

        // Get non-existing storage key for non-existing account and set value
        let (found, value) = statedb.get_storage_mut(&addr_b, &Word::from(3));
        assert!(!found);
        assert_eq!(value, &Word::zero());
        *value = Word::from(102);

        // Get existing account and check nonce
        let (found, acc) = statedb.get_account(&addr_b);
        assert!(found);
        assert_eq!(acc.nonce, Word::zero());

        // Get existing storage key and check value
        let (found, value) = statedb.get_storage(&addr_b, &Word::from(3));
        assert!(found);
        assert_eq!(value, &Word::from(102));
    }
}
