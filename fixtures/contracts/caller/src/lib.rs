#![no_std]

use soroban_sdk::{contract, contractimpl, symbol_short, Address, Env, IntoVal};

#[contract]
pub struct Caller;

#[contractimpl]
impl Caller {
    pub fn call_add(env: Env, target: Address, a: i32, b: i32) -> i32 {
        env.invoke_contract(&target, &symbol_short!("add"), (a, b).into_val(&env))
    }
}
