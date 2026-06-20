#![no_std]

//! # AralPay — programmable tuition escrow for OFW remittances
//!
//! *Aral* means "to study" in Filipino. AralPay lets an overseas Filipino worker
//! send tuition that is **money only their student can turn into tuition** — never
//! cash a household member can divert.
//!
//! The parent (`sponsor`) deposits USDC into an escrow earmarked for a specific
//! `student` and a specific `university`. The funds sit in the contract — out of
//! reach of anyone at home — until the student themselves releases them, at which
//! point the contract verifies the destination against an on-chain registry of
//! real universities and pays the school directly. If the student never enrolls,
//! the parent reclaims the funds after a deadline.
//!
//! MVP flow (demo-able in under 2 minutes):
//!   operator registers a university -> sponsor calls `deposit_tuition`
//!   -> student calls `pay_tuition` -> contract resolves the school's wallet from
//!   the registry and releases the full amount to it.
//!
//! Safety path: after the deadline, the sponsor calls `refund` to recover funds
//! from an escrow the student never released.

use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short, token, Address, Env, String, Symbol,
};

/// Instance-storage flag set the first time `initialize` runs; blocks re-init.
const INITIALIZED: Symbol = symbol_short!("INIT");

/// Global contract configuration, fixed at `initialize`.
#[contracttype]
#[derive(Clone)]
pub struct Config {
    /// Operator that curates the university registry (and nothing else).
    pub admin: Address,
    /// The single asset all escrows are denominated in (e.g. USDC on Stellar).
    pub token: Address,
}

/// Lifecycle of a single tuition escrow.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Funded by the sponsor; awaiting the student's release or a post-deadline refund.
    Funded,
    /// Released to the university.
    Paid,
    /// Returned to the sponsor after the deadline.
    Refunded,
}

/// A single tuition escrow: who funded it, who may release it, where it can go.
#[contracttype]
#[derive(Clone)]
pub struct Escrow {
    /// Sequential escrow id.
    pub id: u64,
    /// The OFW parent who funded the escrow and may refund it after the deadline.
    pub sponsor: Address,
    /// The only party allowed to release the funds to the school.
    pub student: Address,
    /// Registered university name; resolved to a wallet from the registry at pay time.
    pub university: String,
    /// Amount held, in the token's base units.
    pub amount: i128,
    /// Unix time after which an unreleased escrow may be refunded to the sponsor.
    pub deadline: u64,
    /// Current lifecycle state.
    pub status: Status,
}

/// Storage keys.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// -> Config (instance)
    Config,
    /// -> u64, id assigned to the next escrow (instance)
    NextId,
    /// id -> Escrow (persistent)
    Escrow(u64),
    /// university name -> wallet Address (persistent)
    University(String),
}

#[contract]
pub struct AralPayContract;

#[contractimpl]
impl AralPayContract {
    /// One-time setup. Sets the registry operator and the escrow asset.
    pub fn initialize(env: Env, admin: Address, token: Address) {
        if env.storage().instance().has(&INITIALIZED) {
            panic!("already initialized");
        }
        admin.require_auth();
        env.storage().instance().set(&INITIALIZED, &true);
        env.storage()
            .instance()
            .set(&DataKey::Config, &Config { admin, token });
        env.storage().instance().set(&DataKey::NextId, &0u64);
    }

    // --------------------------- Registry (operator) ---------------------------

    /// Operator adds (or updates) a verified university wallet. Only escrows
    /// pointing at a registered name can ever be funded or paid, so funds can
    /// only ever reach a real, vetted school.
    pub fn register_university(env: Env, admin: Address, name: String, wallet: Address) {
        let config = Self::load_config(&env);
        if admin != config.admin {
            panic!("only the operator can manage the registry");
        }
        admin.require_auth();
        env.storage()
            .persistent()
            .set(&DataKey::University(name.clone()), &wallet);
        env.events().publish((symbol_short!("uni_reg"), wallet), name);
    }

    /// Operator removes a university from the registry.
    pub fn remove_university(env: Env, admin: Address, name: String) {
        let config = Self::load_config(&env);
        if admin != config.admin {
            panic!("only the operator can manage the registry");
        }
        admin.require_auth();
        env.storage()
            .persistent()
            .remove(&DataKey::University(name));
    }

    // ------------------------------- Escrow flow -------------------------------

    /// The sponsor (OFW parent) deposits tuition into a new escrow earmarked for
    /// `student` and `university`. The USDC moves straight into the contract's
    /// custody — no household member can touch it. Returns the new escrow id.
    pub fn deposit_tuition(
        env: Env,
        sponsor: Address,
        student: Address,
        university: String,
        amount: i128,
        deadline: u64,
    ) -> u64 {
        let config = Self::load_config(&env);
        sponsor.require_auth();

        if amount <= 0 {
            panic!("amount must be positive");
        }
        // Earmark only to a registered school, so funds can never target a random wallet.
        if !env
            .storage()
            .persistent()
            .has(&DataKey::University(university.clone()))
        {
            panic!("university not registered");
        }
        // A future deadline guarantees the student a real enrollment window.
        if deadline <= env.ledger().timestamp() {
            panic!("deadline must be in the future");
        }

        // Pull the tuition from the sponsor into escrow. Stellar's sub-cent fee and
        // ~5s finality make a cross-border tuition transfer cheap and near-instant.
        token::Client::new(&env, &config.token).transfer(
            &sponsor,
            &env.current_contract_address(),
            &amount,
        );

        let id = Self::read_next_id(&env);
        let escrow = Escrow {
            id,
            sponsor: sponsor.clone(),
            student,
            university,
            amount,
            deadline,
            status: Status::Funded,
        };
        env.storage().persistent().set(&DataKey::Escrow(id), &escrow);
        env.storage().instance().set(&DataKey::NextId, &(id + 1));

        env.events()
            .publish((symbol_short!("deposit"), sponsor), (id, amount));
        id
    }

    /// The student releases their escrow. The contract checks the destination
    /// against the on-chain registry at this moment, then sends the full amount
    /// straight to the university — the only place it can possibly go.
    pub fn pay_tuition(env: Env, student: Address, escrow_id: u64) {
        let config = Self::load_config(&env);
        let mut escrow = Self::load_escrow(&env, escrow_id);

        // Only a still-funded escrow can be released.
        match escrow.status {
            Status::Funded => {}
            _ => panic!("escrow is not in a funded state"),
        }
        // Only the named student may release the funds — never a household member.
        student.require_auth();
        if escrow.student != student {
            panic!("only the student can release this escrow");
        }

        // Resolve the school's wallet from the registry AT PAY TIME, then transfer.
        let university: Address = env
            .storage()
            .persistent()
            .get(&DataKey::University(escrow.university.clone()))
            .expect("university no longer registered");

        // Release the pot from the contract's own balance to the verified school.
        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &university,
            &escrow.amount,
        );

        escrow.status = Status::Paid;
        env.storage()
            .persistent()
            .set(&DataKey::Escrow(escrow_id), &escrow);

        env.events()
            .publish((symbol_short!("paid"), university), (escrow_id, escrow.amount));
    }

    /// After the deadline, the sponsor reclaims an escrow the student never
    /// released — so funds are never permanently stuck if enrollment falls through.
    pub fn refund(env: Env, sponsor: Address, escrow_id: u64) {
        let config = Self::load_config(&env);
        let mut escrow = Self::load_escrow(&env, escrow_id);

        match escrow.status {
            Status::Funded => {}
            _ => panic!("escrow is not in a funded state"),
        }
        sponsor.require_auth();
        if escrow.sponsor != sponsor {
            panic!("only the sponsor can refund this escrow");
        }
        // Refunds open only once the enrollment window has closed.
        if env.ledger().timestamp() < escrow.deadline {
            panic!("deadline not reached");
        }

        token::Client::new(&env, &config.token).transfer(
            &env.current_contract_address(),
            &sponsor,
            &escrow.amount,
        );

        escrow.status = Status::Refunded;
        env.storage()
            .persistent()
            .set(&DataKey::Escrow(escrow_id), &escrow);

        env.events()
            .publish((symbol_short!("refund"), sponsor), (escrow_id, escrow.amount));
    }

    // -------------------------------- Views --------------------------------

    /// Full escrow record.
    pub fn get_escrow(env: Env, escrow_id: u64) -> Escrow {
        Self::load_escrow(&env, escrow_id)
    }

    /// The wallet a university name resolves to, if registered.
    pub fn get_university(env: Env, name: String) -> Option<Address> {
        env.storage().persistent().get(&DataKey::University(name))
    }

    /// Whether a university name is in the registry.
    pub fn is_registered(env: Env, name: String) -> bool {
        env.storage().persistent().has(&DataKey::University(name))
    }

    /// The id that will be assigned to the next escrow (== number created so far).
    pub fn next_escrow_id(env: Env) -> u64 {
        Self::read_next_id(&env)
    }

    /// Contract configuration.
    pub fn get_config(env: Env) -> Config {
        Self::load_config(&env)
    }

    // ------------------------------ Internals ------------------------------

    fn load_config(env: &Env) -> Config {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .expect("contract not initialized")
    }

    fn read_next_id(env: &Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::NextId)
            .unwrap_or(0u64)
    }

    fn load_escrow(env: &Env, id: u64) -> Escrow {
        env.storage()
            .persistent()
            .get(&DataKey::Escrow(id))
            .expect("escrow does not exist")
    }
}

#[cfg(test)]
mod test;