use holo_hash::*;
use holochain_p2p::event::GetActivityOptions;
use holochain_sqlite::rusqlite::*;
use holochain_state::{prelude::*, query::QueryData};
use std::fmt::Debug;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct GetAgentActivityQuery {
    agent: AgentPubKey,
    filter: ChainQueryFilter,
    options: GetActivityOptions,
}

impl GetAgentActivityQuery {
    pub fn new(agent: AgentPubKey, filter: ChainQueryFilter, options: GetActivityOptions) -> Self {
        Self {
            agent,
            filter,
            options,
        }
    }
}

#[derive(Debug, Default)]
pub struct State {
    valid: Vec<ActionHashed>,
    rejected: Vec<ActionHashed>,
    pending: Vec<ActionHashed>,
    status: Option<ChainStatus>,
}

#[derive(Debug)]
pub enum Item {
    Integrated(ActionHashed),
    Pending(ActionHashed),
}

impl Query for GetAgentActivityQuery {
    type Item = Judged<Item>;
    type State = State;
    type Output = AgentActivityResponse<ActionHash>;

    fn query(&self) -> String {
        "
            SELECT Action.hash, DhtOp.validation_status, Action.blob AS action_blob,
            DhtOp.when_integrated
            FROM Action
            JOIN DhtOp ON DhtOp.action_hash = Action.hash
            WHERE Action.author = :author
            AND DhtOp.type = :op_type
            ORDER BY Action.seq ASC
        "
        .to_string()
    }

    fn params(&self) -> Vec<holochain_state::query::Params> {
        (named_params! {
            ":author": self.agent,
            ":op_type": ChainOpType::RegisterAgentActivity,
        })
        .to_vec()
    }

    fn init_fold(&self) -> StateQueryResult<Self::State> {
        Ok(Default::default())
    }

    fn as_filter(&self) -> Box<dyn Fn(&QueryData<Self>) -> bool> {
        unimplemented!("This query should not be used with the scratch")
    }

    fn as_map(&self) -> Arc<dyn Fn(&Row) -> StateQueryResult<Self::Item>> {
        Arc::new(move |row| {
            let validation_status: Option<ValidationStatus> = row.get("validation_status")?;
            let hash: ActionHash = row.get("hash")?;
            from_blob::<SignedAction>(row.get("action_blob")?).and_then(|action| {
                let integrated: Option<Timestamp> = row.get("when_integrated")?;
                let action = ActionHashed::with_pre_hashed(action.into_data(), hash);
                let item = if integrated.is_some() {
                    Item::Integrated(action)
                } else {
                    Item::Pending(action)
                };
                Ok(Judged::raw(item, validation_status))
            })
        })
    }

    fn fold(&self, mut state: Self::State, item: Self::Item) -> StateQueryResult<Self::State> {
        let status = item.validation_status();
        match (status, item.data) {
            (Some(ValidationStatus::Valid), Item::Integrated(action)) => {
                let seq = action.action_seq();
                if state.status.is_none() {
                    let fork = state.valid.last().and_then(|v| {
                        if seq == v.action_seq() {
                            Some(v)
                        } else {
                            None
                        }
                    });
                    if let Some(fork) = fork {
                        state.status = Some(ChainStatus::Forked(ChainFork {
                            fork_seq: seq,
                            first_action: action.get_hash().clone(),
                            second_action: fork.get_hash().clone(),
                        }));
                    }
                }

                state.valid.push(action);
            }
            (Some(ValidationStatus::Rejected), Item::Integrated(action)) => {
                if state.status.is_none() {
                    state.status = Some(ChainStatus::Invalid(ChainHead {
                        action_seq: action.action_seq(),
                        hash: action.get_hash().clone(),
                    }));
                }
                state.rejected.push(action);
            }
            (_, Item::Pending(data)) => state.pending.push(data),
            _ => (),
        }
        Ok(state)
    }

    fn render<S>(&self, state: Self::State, _stores: S) -> StateQueryResult<Self::Output>
    where
        S: Store,
    {
        let highest_observed = compute_highest_observed(&state);
        let status = compute_chain_status(&state);

        let valid = state.valid;
        let rejected = state.rejected;
        let valid_activity = if self.options.include_valid_activity {
            let valid = self
                .filter
                .filter_actions(valid)
                .into_iter()
                .map(|h| (h.action_seq(), h.into_hash()))
                .collect();
            ChainItems::Hashes(valid)
        } else {
            ChainItems::NotRequested
        };
        let rejected_activity = if self.options.include_rejected_activity {
            let rejected = self
                .filter
                .filter_actions(rejected)
                .into_iter()
                .map(|h| (h.action_seq(), h.into_hash()))
                .collect();
            ChainItems::Hashes(rejected)
        } else {
            ChainItems::NotRequested
        };

        Ok(AgentActivityResponse {
            agent: self.agent.clone(),
            valid_activity,
            rejected_activity,
            status,
            highest_observed,
        })
    }
}

fn compute_chain_status(state: &State) -> ChainStatus {
    state.status.clone().unwrap_or_else(|| {
        if state.valid.is_empty() && state.rejected.is_empty() {
            ChainStatus::Empty
        } else {
            let last = state.valid.last().expect("Safe due to is_empty check");
            ChainStatus::Valid(ChainHead {
                action_seq: last.action_seq(),
                hash: last.get_hash().clone(),
            })
        }
    })
}

fn compute_highest_observed(state: &State) -> Option<HighestObserved> {
    let mut highest_observed = None;
    let mut hashes = Vec::new();
    let mut check_highest = |seq: u32, hash: &ActionHash| {
        if highest_observed.is_none() {
            highest_observed = Some(seq);
            hashes.push(hash.clone());
        } else {
            let last = highest_observed
                .as_mut()
                .expect("Safe due to none check above");
            match seq.cmp(last) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => hashes.push(hash.clone()),
                std::cmp::Ordering::Greater => {
                    hashes.clear();
                    hashes.push(hash.clone());
                    *last = seq;
                }
            }
        }
    };
    if let Some(valid) = state.valid.last() {
        check_highest(valid.action_seq(), valid.get_hash());
    }
    if let Some(rejected) = state.rejected.last() {
        check_highest(rejected.action_seq(), rejected.get_hash());
    }
    if let Some(pending) = state.pending.last() {
        check_highest(pending.action_seq(), pending.get_hash());
    }
    highest_observed.map(|action_seq| HighestObserved {
        action_seq,
        hash: hashes,
    })
}
