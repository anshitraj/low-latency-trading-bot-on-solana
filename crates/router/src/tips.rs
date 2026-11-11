//! Tip planning across a set of senders racing the same transaction.
//!
//! A tip is a SystemProgram transfer *inside* the signed transaction, so it is
//! paid exactly once if (and only if) the transaction executes successfully,
//! regardless of which path delivered it. Because the bytes are shared, all
//! racing senders must accept the same tip destination; incompatible sets are
//! refused instead of silently paying an unintended tip or sending a tx a
//! sender will drop.

use serde::Serialize;
use solana_pubkey::Pubkey;
use trader_domain::SenderKind;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TipPolicy {
    /// Sender ignores tips (plain RPC, TPU).
    None,
    /// Tip improves priority but is not required (Jito sendTransaction).
    Optional {
        min_lamports: u64,
        #[serde(skip)]
        accounts: Vec<Pubkey>,
    },
    /// Sender drops transactions without a qualifying tip (Helius Sender).
    Required {
        min_lamports: u64,
        #[serde(skip)]
        accounts: Vec<Pubkey>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TipPlan {
    pub account: Pubkey,
    pub lamports: u64,
    pub for_sender: SenderKind,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TipPlanError {
    #[error("sender {sender:?} requires a tip of at least {min} lamports but user cap is {cap}")]
    CapTooLow {
        sender: SenderKind,
        min: u64,
        cap: u64,
    },
    #[error("senders {a:?} and {b:?} require tips to different account sets; choose one")]
    Incompatible { a: SenderKind, b: SenderKind },
    #[error("sender {0:?} has no tip accounts configured")]
    NoAccounts(SenderKind),
}

fn pick(accounts: &[Pubkey], salt: u64) -> Option<Pubkey> {
    if accounts.is_empty() {
        None
    } else {
        Some(accounts[(salt as usize) % accounts.len()])
    }
}

/// Returns the tip to embed (or None). `desired` is the user's requested tip,
/// `cap` the hard per-user ceiling. Deterministic for a given input.
pub fn plan_tip(
    senders: &[(SenderKind, TipPolicy)],
    desired: u64,
    cap: u64,
) -> Result<Option<TipPlan>, TipPlanError> {
    let required: Vec<&(SenderKind, TipPolicy)> = senders
        .iter()
        .filter(|(_, p)| matches!(p, TipPolicy::Required { .. }))
        .collect();
    if required.len() > 1 {
        let (a, pa) = required[0];
        let (b, pb) = required[1];
        let (TipPolicy::Required { accounts: aa, .. }, TipPolicy::Required { accounts: ab, .. }) =
            (pa, pb)
        else {
            unreachable!()
        };
        if !aa.iter().any(|x| ab.contains(x)) {
            return Err(TipPlanError::Incompatible { a: *a, b: *b });
        }
    }
    let salt = desired ^ (senders.len() as u64);
    if let Some((
        kind,
        TipPolicy::Required {
            min_lamports,
            accounts,
        },
    )) = required.first().map(|x| (&x.0, &x.1))
    {
        if *min_lamports > cap {
            return Err(TipPlanError::CapTooLow {
                sender: *kind,
                min: *min_lamports,
                cap,
            });
        }
        let amount = desired.max(*min_lamports).min(cap);
        let account = pick(accounts, salt).ok_or(TipPlanError::NoAccounts(*kind))?;
        // An optional-tip sender in the same race with a *different* account set
        // just gets no tip credit; that's acceptable (it still forwards).
        return Ok(Some(TipPlan {
            account,
            lamports: amount,
            for_sender: *kind,
        }));
    }
    if desired == 0 {
        return Ok(None);
    }
    for (kind, p) in senders {
        if let TipPolicy::Optional {
            min_lamports,
            accounts,
        } = p
        {
            let amount = desired.min(cap);
            if amount < *min_lamports {
                continue; // below that sender's minimum: no point tipping it
            }
            let account = pick(accounts, salt).ok_or(TipPlanError::NoAccounts(*kind))?;
            return Ok(Some(TipPlan {
                account,
                lamports: amount,
                for_sender: *kind,
            }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accts(n: u8) -> Vec<Pubkey> {
        (0..3)
            .map(|i| Pubkey::new_from_array([n * 10 + i; 32]))
            .collect()
    }

    #[test]
    fn planning() {
        let rpc = (SenderKind::Rpc, TipPolicy::None);
        let jito = (
            SenderKind::Jito,
            TipPolicy::Optional {
                min_lamports: 1_000,
                accounts: accts(1),
            },
        );
        let helius = (
            SenderKind::Helius,
            TipPolicy::Required {
                min_lamports: 5_000,
                accounts: accts(2),
            },
        );

        assert_eq!(
            plan_tip(std::slice::from_ref(&rpc), 10_000, 10_000).unwrap(),
            None
        );
        // no tip requested -> none even with optional sender
        assert_eq!(
            plan_tip(&[rpc.clone(), jito.clone()], 0, 10_000).unwrap(),
            None
        );
        // optional below min -> none
        assert_eq!(
            plan_tip(std::slice::from_ref(&jito), 500, 10_000).unwrap(),
            None
        );
        let p = plan_tip(&[rpc.clone(), jito.clone()], 2_000, 1_500)
            .unwrap()
            .unwrap();
        assert_eq!((p.lamports, p.for_sender), (1_500, SenderKind::Jito));
        assert!(accts(1).contains(&p.account));
        // required: raised to min, capped
        let p = plan_tip(&[helius.clone(), rpc.clone()], 0, 10_000)
            .unwrap()
            .unwrap();
        assert_eq!(p.lamports, 5_000);
        assert!(accts(2).contains(&p.account));
        assert!(matches!(
            plan_tip(std::slice::from_ref(&helius), 0, 4_999),
            Err(TipPlanError::CapTooLow { .. })
        ));
        // two required senders with disjoint accounts
        let other = (
            SenderKind::Tpu,
            TipPolicy::Required {
                min_lamports: 1,
                accounts: accts(3),
            },
        );
        assert!(matches!(
            plan_tip(&[helius.clone(), other], 0, 10_000),
            Err(TipPlanError::Incompatible { .. })
        ));
        let empty = (
            SenderKind::Helius,
            TipPolicy::Required {
                min_lamports: 1,
                accounts: vec![],
            },
        );
        assert!(matches!(
            plan_tip(&[empty], 0, 10),
            Err(TipPlanError::NoAccounts(_))
        ));
    }
}
