use me_core::symbol::SymbolConfig;
use me_financial::{transport::TlsFiles, Fees};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Matcher {
    pub address: String,
    pub server_name: String,
    pub certificate_fingerprint: String,
    pub tls: TlsFiles,
}
#[derive(Clone, Copy, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Read,
    Trade,
    Funding,
    Admin,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    #[serde(default)]
    pub accounts: BTreeSet<Uuid>,
    #[serde(default)]
    pub all_accounts: bool,
    pub permissions: BTreeSet<Permission>,
}
impl Principal {
    pub fn permits(&self, p: Permission, account: Option<Uuid>) -> bool {
        self.permissions.contains(&p)
            && account.is_none_or(|a| self.all_accounts || self.accounts.contains(&a))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: String,
    pub tls: TlsFiles,
    pub matcher: Matcher,
    pub principals: BTreeMap<String, Principal>,
    pub symbols: Vec<SymbolConfig>,
    pub fees: BTreeMap<u16, Fees>,
    #[serde(default)]
    pub assets: BTreeMap<String, Option<u8>>,
    #[serde(default = "database_env")]
    pub database_url_env: String,
}
fn database_env() -> String {
    "ME_LEDGER_DATABASE_URL".into()
}
