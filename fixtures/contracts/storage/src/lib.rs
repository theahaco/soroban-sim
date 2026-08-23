#![no_std]

use soroban_sdk::{contract, contractimpl, symbol_short, Env, Symbol};

const INSTANCE: Symbol = symbol_short!("INST");

#[contract]
pub struct Storage;

#[contractimpl]
impl Storage {
    pub fn put_persist(env: Env, key: u32, val: u32) {
        env.storage().persistent().set(&key, &val);
    }

    pub fn get_persist(env: Env, key: u32) -> u32 {
        env.storage().persistent().get(&key).unwrap()
    }

    pub fn put_temp(env: Env, key: u32, val: u32) {
        env.storage().temporary().set(&key, &val);
    }

    pub fn get_temp(env: Env, key: u32) -> u32 {
        env.storage().temporary().get(&key).unwrap()
    }

    pub fn set_instance(env: Env, val: u32) {
        env.storage().instance().set(&INSTANCE, &val);
    }

    pub fn get_instance(env: Env) -> u32 {
        env.storage().instance().get(&INSTANCE).unwrap()
    }
}
