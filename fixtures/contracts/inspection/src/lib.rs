#![no_std]
use soroban_sdk::{
    auth::{Context, CustomAccountInterface},
    contract, contracterror, contractimpl,
    crypto::Hash,
    panic_with_error, symbol_short, Address, Env, IntoVal, Vec,
};
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    Refused = 17,
    Inner = 23,
}
#[contract]
pub struct Inspection;
#[contractimpl]
impl Inspection {
    pub fn inner(env: Env, account: Address, value: u32) {
        account.require_auth();
        env.storage().persistent().set(&value, &value);
    }
    pub fn outer(env: Env, account: Address, child: Address, value: u32) {
        account.require_auth();
        env.invoke_contract::<()>(
            &child,
            &symbol_short!("inner"),
            (account, value).into_val(&env),
        );
    }
    pub fn nonroot(env: Env, account: Address, child: Address, value: u32) {
        env.invoke_contract::<()>(
            &child,
            &symbol_short!("inner"),
            (account, value).into_val(&env),
        );
    }
    pub fn fail(env: Env, value: u32, fail: bool) {
        env.storage().persistent().set(&value, &value);
        if fail {
            panic_with_error!(&env, Error::Inner);
        }
    }
    pub fn rollback(env: Env, child: Address, value: u32, catch: bool, fail: bool) {
        env.storage().persistent().set(&value, &value);
        env.storage().temporary().set(&value, &value);
        env.storage().instance().set(&value, &value);
        env.storage().persistent().has(&value);
        env.storage().persistent().get::<_, u32>(&value);
        env.storage().temporary().remove(&value);
        env.storage().persistent().extend_ttl(&value, 100, 200);
        if catch {
            let _ = env.try_invoke_contract::<(), Error>(
                &child,
                &symbol_short!("fail"),
                (value, fail).into_val(&env),
            );
        } else {
            env.invoke_contract::<()>(&child, &symbol_short!("fail"), (value, fail).into_val(&env));
        }
    }
}
#[contractimpl]
impl CustomAccountInterface for Inspection {
    type Signature = (bool, Vec<Address>);
    type Error = Error;
    fn __check_auth(
        env: Env,
        _payload: Hash<32>,
        signature: Self::Signature,
        _contexts: Vec<Context>,
    ) -> Result<(), Error> {
        if !signature.0 {
            return Err(Error::Refused);
        }
        for delegate in signature.1 {
            env.custom_account().delegate_auth(&delegate);
        }
        Ok(())
    }
}
