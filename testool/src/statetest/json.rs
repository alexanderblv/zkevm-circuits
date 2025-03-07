#![allow(dead_code, unused_imports)]

use super::{
    parse,
    spec::{AccountMatch, Env, StateTest, DEFAULT_BASE_FEE},
};
use crate::{abi, compiler::Compiler, utils::MainnetFork};
use anyhow::{bail, Context, Result};
use eth_types::{evm_types::OpcodeId, geth_types::Account, Address, Bytes, H256, U256};
use ethers_core::{k256::ecdsa::SigningKey, utils::secret_key_to_address};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    convert::TryInto,
    ops::RangeBounds,
    str::FromStr,
};
use yaml_rust::Yaml;

fn default_block_base_fee() -> String {
    DEFAULT_BASE_FEE.to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestEnv {
    #[serde(default = "default_block_base_fee")]
    current_base_fee: String,
    current_coinbase: String,
    #[serde(default)]
    current_difficulty: String,
    current_gas_limit: String,
    current_number: String,
    current_timestamp: String,
    #[serde(default)]
    previous_hash: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Indexes {
    #[serde(default)]
    data: serde_json::value::Value,
    #[serde(default)]
    gas: serde_json::value::Value,
    #[serde(default)]
    value: serde_json::value::Value,
}

#[derive(Debug, Clone, Deserialize)]
struct AccountPost {
    balance: Option<String>,
    code: Option<String>,
    nonce: Option<String>,
    storage: Option<HashMap<String, String>>,
    shouldnotexist: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AccountPre {
    balance: String,
    code: String,
    nonce: String,
    storage: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Expect {
    indexes: Option<Indexes>,
    network: Vec<String>,
    result: BTreeMap<String, AccountPost>,
}

#[derive(Debug, Clone, Deserialize)]
struct JsonStateTest {
    env: TestEnv,
    transaction: Transaction,
    pre: HashMap<String, AccountPre>,
    expect: Vec<Expect>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Transaction {
    access_list: Option<parse::RawAccessList>,
    data: Vec<String>,
    gas_limit: Vec<String>,
    max_priority_fee_per_gas: Option<String>,
    max_fee_per_gas: Option<String>,
    gas_price: String,
    nonce: String,
    secret_key: String,
    to: String,
    value: Vec<String>,
}

#[derive(Debug, Clone)]
enum Ref {
    Any,
    Index(usize),
}

struct Refs(Vec<Ref>);

impl Refs {
    fn contains_index(&self, idx: usize) -> bool {
        self.0.iter().any(|r| match r {
            Ref::Index(i) => i == &idx,
            Ref::Any => true,
        })
    }
}

pub struct JsonStateTestBuilder<'a> {
    compiler: &'a Compiler,
}

impl<'a> JsonStateTestBuilder<'a> {
    pub fn new(compiler: &'a Compiler) -> Self {
        Self { compiler }
    }

    /// generates `StateTest` vectors from a ethereum josn test specification
    pub fn load_json(&mut self, path: &str, source: &str) -> Result<Vec<StateTest>> {
        let mut state_tests = Vec::new();
        let tests: HashMap<String, JsonStateTest> = serde_json::from_str(source).unwrap();

        for (test_name, test) in tests {
            let env = Self::parse_env(&test.env).unwrap();
            let pre = self.parse_accounts_pre(&test.pre).unwrap();

            let to = parse::parse_to_address(&test.transaction.to).unwrap();
            let secret_key = parse::parse_bytes(&test.transaction.secret_key).unwrap();
            let from = secret_key_to_address(&SigningKey::from_slice(&secret_key).unwrap());
            let nonce = parse::parse_u256(&test.transaction.nonce).unwrap();

            let max_priority_fee_per_gas = test
                .transaction
                .max_priority_fee_per_gas
                .map_or(Ok(None), |s| parse::parse_u256(&s).map(Some))
                .unwrap();
            let max_fee_per_gas = test
                .transaction
                .max_fee_per_gas
                .map_or(Ok(None), |s| parse::parse_u256(&s).map(Some))
                .unwrap();

            // Set gas price to `min(max_priority_fee_per_gas + base_fee, max_fee_per_gas)` for
            // EIP-1559 transaction.
            // <https://github.com/ethereum/go-ethereum/blob/1485814f89d8206bb4a1c8e10a4a2893920f683a/core/state_transition.go#L167>
            let gas_price = parse::parse_u256(&test.transaction.gas_price).unwrap_or_else(|_| {
                max_fee_per_gas
                    .unwrap()
                    .min(max_priority_fee_per_gas.unwrap() + env.current_base_fee)
            });

            let access_list = &test.transaction.access_list;

            let data_s: Vec<_> = test
                .transaction
                .data
                .iter()
                .map(|item| parse::parse_calldata(self.compiler, item, access_list))
                .collect::<Result<_>>()
                .unwrap();

            let gas_limit_s: Vec<_> = test
                .transaction
                .gas_limit
                .iter()
                .map(|item| parse::parse_u64(item))
                .collect::<Result<_>>()
                .unwrap();

            let value_s: Vec<_> = test
                .transaction
                .value
                .iter()
                .map(|item| parse::parse_u256(item))
                .collect::<Result<_>>()
                .unwrap();

            let mut expects = Vec::new();
            for expect in test.expect {
                // Considered as Anys if missing `indexes`.
                let (data_refs, gas_refs, value_refs) = if let Some(indexes) = expect.indexes {
                    (
                        Self::parse_refs(&indexes.data).unwrap(),
                        Self::parse_refs(&indexes.gas).unwrap(),
                        Self::parse_refs(&indexes.value).unwrap(),
                    )
                } else {
                    (
                        Refs(vec![Ref::Any]),
                        Refs(vec![Ref::Any]),
                        Refs(vec![Ref::Any]),
                    )
                };

                let result = self.parse_accounts_post(&expect.result).unwrap();

                if MainnetFork::in_network_range(&expect.network).unwrap() {
                    expects.push((data_refs, gas_refs, value_refs, result));
                }
            }

            for (idx_data, calldata) in data_s.iter().enumerate() {
                for (idx_gas, gas_limit) in gas_limit_s.iter().enumerate() {
                    for (idx_value, value) in value_s.iter().enumerate() {
                        for (data_refs, gas_refs, value_refs, result) in &expects {
                            if !data_refs.contains_index(idx_data) {
                                continue;
                            }

                            if !gas_refs.contains_index(idx_gas) {
                                continue;
                            }

                            if !value_refs.contains_index(idx_value) {
                                continue;
                            }

                            state_tests.push(StateTest {
                                path: path.to_string(),
                                id: format!("{test_name}_d{idx_data}_g{idx_gas}_v{idx_value}"),
                                env: env.clone(),
                                pre: pre.clone(),
                                result: result.clone(),
                                from,
                                to,
                                secret_key: secret_key.clone(),
                                nonce,
                                max_priority_fee_per_gas,
                                max_fee_per_gas,
                                gas_price,
                                gas_limit: *gas_limit,
                                value: *value,
                                data: calldata.data.clone(),
                                access_list: calldata.access_list.clone(),
                                exception: false,
                            });
                        }
                    }
                }
            }
        }

        Ok(state_tests)
    }

    /// parse env section
    fn parse_env(env: &TestEnv) -> Result<Env> {
        Ok(Env {
            current_base_fee: parse::parse_u256(&env.current_base_fee)
                .unwrap_or_else(|_| U256::from(DEFAULT_BASE_FEE)),
            current_coinbase: parse::parse_address(&env.current_coinbase)?,
            current_difficulty: parse::parse_u256(&env.current_difficulty).unwrap_or_default(),
            current_gas_limit: parse::parse_u64(&env.current_gas_limit)?,
            current_number: parse::parse_u64(&env.current_number)?,
            current_timestamp: parse::parse_u64(&env.current_timestamp)?,
            previous_hash: parse::parse_hash(&env.previous_hash).unwrap_or_default(),
        })
    }

    /// parse a vector of address=>(storage,balance,code,nonce) entry
    fn parse_accounts_pre(
        &mut self,
        accounts_pre: &HashMap<String, AccountPre>,
    ) -> Result<BTreeMap<Address, Account>> {
        let mut accounts = BTreeMap::new();
        for (address, acc) in accounts_pre {
            let address = parse::parse_address(address)?;
            let mut storage = HashMap::new();
            for (k, v) in &acc.storage {
                storage.insert(parse::parse_u256(k)?, parse::parse_u256(v)?);
            }
            let account = Account {
                address,
                balance: parse::parse_u256(&acc.balance)?,
                nonce: parse::parse_u256(&acc.nonce)?,
                code: parse::parse_code(self.compiler, &acc.code)?,
                storage,
            };
            accounts.insert(address, account);
        }
        Ok(accounts)
    }

    /// parse a vector of address=>(storage,balance,code,nonce) entry
    fn parse_accounts_post(
        &mut self,
        accounts_post: &BTreeMap<String, AccountPost>,
    ) -> Result<BTreeMap<Address, AccountMatch>> {
        let mut accounts = BTreeMap::new();
        for (address, acc) in accounts_post {
            let address = parse::parse_address(address)?;
            let mut storage: HashMap<U256, U256> = HashMap::new();
            if let Some(acc_storage) = &acc.storage {
                for (k, v) in acc_storage {
                    storage.insert(parse::parse_u256(k)?, parse::parse_u256(v)?);
                }
            }
            let account = AccountMatch {
                address,
                balance: acc
                    .balance
                    .as_ref()
                    .map(|v| parse::parse_u256(v))
                    .transpose()?,
                code: acc
                    .code
                    .as_ref()
                    .map(|v| parse::parse_code(self.compiler, v))
                    .transpose()?,
                nonce: acc
                    .nonce
                    .as_ref()
                    .map(|v| parse::parse_u256(v))
                    .transpose()?,
                storage,
            };
            accounts.insert(address, account);
        }
        Ok(accounts)
    }

    /// parse a unique or a list of references,
    ///   -1 => Ref::Any
    ///   a int value => Ref::Index(value)
    ///   :label xxx => Ref::Label(value)
    ///   <range_lo>-<range_hi> >= Ref::Index(range_lo)..=RefIndex(range_hi)
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn parse_refs(value: &serde_json::Value) -> Result<Refs> {
        let mut refs = Vec::new();
        if value.is_null() {
            return Ok(Refs(vec![Ref::Any]));
        }
        if let Some(index) = value.as_i64() {
            if index == -1 {
                refs.push(Ref::Any);
            } else {
                refs.push(Ref::Index(index as usize));
            }
        } else if let Some(array) = value.as_array() {
            for element in array {
                if let Some(index) = element.as_u64() {
                    refs.push(Ref::Index(index as usize));
                } else {
                    bail!("unable to parse ref: {:?}", value);
                }
            }
        } else {
            bail!("unable to parse ref(2): {:?}", value);
        }
        Ok(Refs(refs))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use eth_types::{address, AccessList, AccessListItem};

    const JSON: &str = r#"
{
    "add11" : {
        "_info" : {
            "comment" : "A test for (add 1 1) opcode result"
        },
        "env" : {
            "currentCoinbase" : "2adc25665018aa1fe0e6bc666dac8fc2697ff9ba",
            "currentDifficulty" : "0x20000",
            "currentGasLimit" : "0xFF112233445566",
            "currentNumber" : "1",
            "currentTimestamp" : "1000",
            "previousHash" : "5e20a0453cecd065ea59c37ac63e079ee08998b6045136a8ce6635c7912ec0b6"
        },
        "expect" : [
            {
                "indexes" : {
                    "data" : -1,
                    "gas" : -1,
                    "value" : -1
                },
                "network" : [">=Berlin"],
                "result" : {
                    "095e7baea6a6c7c4c2dfeb977efac326af552d87" : {
                        "code" : "0x600160010160005500",
                        "nonce" : "1",
                        "storage" : {
                            "0x00" : "0x02"
                        }
                    }
                }
            }
        ],
        "pre" : {
            "095e7baea6a6c7c4c2dfeb977efac326af552d87" : {
                "balance" : "1000000000000000000",
                "code" : "0x600160010160005500",
                "nonce" : "0",
                "storage" : {
                }
            }
        },
        "transaction" : {
            "accessList" : [
                {
                    "address" : "0x009e7baea6a6c7c4c2dfeb977efac326af552d87",
                    "storageKeys" : [
                        "0x0000000000000000000000000000000000000000000000000000000000000000",
                        "0x0000000000000000000000000000000000000000000000000000000000000001"
                    ]
                }
            ],
            "data" : [
                "0x6001",
                "0x6002"
            ],
            "gasLimit" : [
                "400000"
            ],
            "gasPrice" : "10",
            "nonce" : "0",
            "secretKey" : "45a915e4d060149eb4365960e6a7a45f334393093061116b197e3240065ff2d8",
            "to" : "095e7baea6a6c7c4c2dfeb977efac326af552d87",
            "value" : [
                "100000"
            ]
        }
    }
}
"#;
    #[test]
    fn test_json_parse() -> Result<()> {
        let compiler = Compiler::new(true, None)?;
        let mut builder = JsonStateTestBuilder::new(&compiler);
        let test = builder.load_json("test_path", JSON)?.remove(0);

        let acc095e = Address::from_str("0x095e7baea6a6c7c4c2dfeb977efac326af552d87")?;

        let expected = StateTest {
            path: "test_path".to_string(),
            id: "add11_d0_g0_v0".to_string(),
            env: Env {
                current_base_fee: U256::from(DEFAULT_BASE_FEE),
                current_coinbase: Address::from_str("0x2adc25665018aa1fe0e6bc666dac8fc2697ff9ba")?,
                current_difficulty: U256::from(131072u64),
                current_gas_limit: 0xFF112233445566,
                current_number: 1,
                current_timestamp: 1000,
                previous_hash: H256::from_str(
                    "0x5e20a0453cecd065ea59c37ac63e079ee08998b6045136a8ce6635c7912ec0b6",
                )?,
            },
            secret_key: Bytes::from(hex::decode(
                "45a915e4d060149eb4365960e6a7a45f334393093061116b197e3240065ff2d8",
            )?),
            from: Address::from_str("0xa94f5374fce5edbc8e2a8697c15331677e6ebf0b")?,
            to: Some(Address::from_str(
                "0x095e7baea6a6c7c4c2dfeb977efac326af552d87",
            )?),
            gas_limit: 400000,
            gas_price: U256::from(10u64),
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
            nonce: U256::from(0u64),
            value: U256::from(100000u64),
            data: Bytes::from(hex::decode("6001")?),
            access_list: Some(AccessList(vec![AccessListItem {
                address: address!("0x009e7baea6a6c7c4c2dfeb977efac326af552d87"),
                storage_keys: vec![
                    H256::from_str(
                        "0x0000000000000000000000000000000000000000000000000000000000000000",
                    )
                    .unwrap(),
                    H256::from_str(
                        "0x0000000000000000000000000000000000000000000000000000000000000001",
                    )
                    .unwrap(),
                ],
            }])),
            pre: BTreeMap::from([(
                acc095e,
                Account {
                    address: acc095e,
                    nonce: U256::from(0u64),
                    balance: U256::from(1000000000000000000u64),
                    code: Bytes::from(hex::decode("600160010160005500")?),
                    storage: HashMap::new(),
                },
            )]),
            result: BTreeMap::from([(
                acc095e,
                AccountMatch {
                    address: acc095e,
                    nonce: Some(U256::from(1u64)),
                    balance: None,
                    code: Some(Bytes::from(hex::decode("600160010160005500")?)),
                    storage: HashMap::from([(U256::zero(), U256::from(2u64))]),
                },
            )]),
            exception: false,
        };

        assert_eq!(expected, test);

        Ok(())
    }
}
