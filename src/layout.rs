use std::collections::BTreeMap;

pub const MAX_MEMBERS: usize = 8;
pub const MAX_TABS: usize = 9;
pub const MAX_PANES_PER_TAB: usize = 8;
pub const MAX_SPLIT_DEPTH: usize = 4;

pub type PaneId = u64;
pub type TabId = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Axis {
    LeftRight,
    TopBottom,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Node {
    Leaf {
        pane_id: PaneId,
    },
    Split {
        axis: Axis,
        first: Box<Node>,
        second: Box<Node>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Member {
    pub peer_id: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pane {
    pub pane_id: PaneId,
    pub host_peer_id: Vec<u8>,
    pub grid_rows: u16,
    pub grid_cols: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tab {
    pub tab_id: TabId,
    pub root: Node,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionState {
    pub revision: u64,
    pub members: Vec<Member>,
    pub tabs: Vec<Tab>,
    pub panes: BTreeMap<PaneId, Pane>,
    next_tab_id: TabId,
    next_pane_id: PaneId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutError {
    StaleRevision { expected: u64, got: u64 },
    RevisionExhausted,
    MemberLimit,
    AlreadyMember,
    NotMember,
    TabLimit,
    PaneLimit,
    SplitDepthLimit,
    InvalidGrid,
    UnknownPane { pane_id: PaneId },
    UnknownTab { tab_id: TabId },
    NotPaneHost { pane_id: PaneId },
    NotTabHost { tab_id: TabId },
    LastPaneInTab { tab_id: TabId },
    LastTab,
    IdExhausted,
}

impl SessionState {
    pub fn new(initial_host: Vec<u8>, grid_rows: u16, grid_cols: u16) -> Result<Self, LayoutError> {
        validate_grid(grid_rows, grid_cols)?;

        let initial_pane = Pane {
            pane_id: 1,
            host_peer_id: initial_host.clone(),
            grid_rows,
            grid_cols,
        };
        let mut panes = BTreeMap::new();
        panes.insert(initial_pane.pane_id, initial_pane);

        Ok(Self {
            revision: 1,
            members: vec![Member {
                peer_id: initial_host,
            }],
            tabs: vec![Tab {
                tab_id: 1,
                root: Node::Leaf { pane_id: 1 },
            }],
            panes,
            next_tab_id: 2,
            next_pane_id: 2,
        })
    }

    pub fn add_member(&mut self, base_revision: u64, peer_id: Vec<u8>) -> Result<(), LayoutError> {
        self.check_mutation(base_revision)?;
        if self.members.iter().any(|member| member.peer_id == peer_id) {
            return Err(LayoutError::AlreadyMember);
        }
        if self.members.len() >= MAX_MEMBERS {
            return Err(LayoutError::MemberLimit);
        }

        self.members.push(Member { peer_id });
        self.advance_revision();
        Ok(())
    }

    pub fn create_pane(
        &mut self,
        requester: &[u8],
        base_revision: u64,
        target_pane_id: PaneId,
        axis: Axis,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<PaneId, LayoutError> {
        self.check_mutation(base_revision)?;
        self.require_member(requester)?;
        validate_grid(grid_rows, grid_cols)?;

        let tab_index =
            self.tab_index_for_pane(target_pane_id)
                .ok_or(LayoutError::UnknownPane {
                    pane_id: target_pane_id,
                })?;
        if self.pane_ids_in_tab_at(tab_index).len() >= MAX_PANES_PER_TAB {
            return Err(LayoutError::PaneLimit);
        }
        let target_depth = self.tabs[tab_index]
            .root
            .leaf_depth(target_pane_id)
            .expect("tab_index_for_pane found the leaf");
        if target_depth >= MAX_SPLIT_DEPTH {
            return Err(LayoutError::SplitDepthLimit);
        }

        let pane_id = self.next_pane_id;
        let next_pane_id = self.next_id(pane_id)?;
        let split = Node::Split {
            axis,
            first: Box::new(Node::Leaf {
                pane_id: target_pane_id,
            }),
            second: Box::new(Node::Leaf { pane_id }),
        };
        let replaced = self.tabs[tab_index]
            .root
            .replace_leaf(target_pane_id, split);
        debug_assert!(replaced);
        self.panes.insert(
            pane_id,
            Pane {
                pane_id,
                host_peer_id: requester.to_vec(),
                grid_rows,
                grid_cols,
            },
        );
        self.next_pane_id = next_pane_id;
        self.advance_revision();
        Ok(pane_id)
    }

    pub fn create_tab(
        &mut self,
        requester: &[u8],
        base_revision: u64,
        grid_rows: u16,
        grid_cols: u16,
    ) -> Result<TabId, LayoutError> {
        self.check_mutation(base_revision)?;
        self.require_member(requester)?;
        validate_grid(grid_rows, grid_cols)?;
        if self.tabs.len() >= MAX_TABS {
            return Err(LayoutError::TabLimit);
        }

        let tab_id = self.next_tab_id;
        let pane_id = self.next_pane_id;
        let next_tab_id = self.next_id(tab_id)?;
        let next_pane_id = self.next_id(pane_id)?;
        self.panes.insert(
            pane_id,
            Pane {
                pane_id,
                host_peer_id: requester.to_vec(),
                grid_rows,
                grid_cols,
            },
        );
        self.tabs.push(Tab {
            tab_id,
            root: Node::Leaf { pane_id },
        });
        self.next_tab_id = next_tab_id;
        self.next_pane_id = next_pane_id;
        self.advance_revision();
        Ok(tab_id)
    }

    pub fn delete_pane(
        &mut self,
        requester: &[u8],
        base_revision: u64,
        pane_id: PaneId,
    ) -> Result<(), LayoutError> {
        self.check_mutation(base_revision)?;
        self.require_member(requester)?;
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or(LayoutError::UnknownPane { pane_id })?;
        if pane.host_peer_id != requester {
            return Err(LayoutError::NotPaneHost { pane_id });
        }
        let tab_index = self
            .tab_index_for_pane(pane_id)
            .ok_or(LayoutError::UnknownPane { pane_id })?;
        if self.pane_ids_in_tab_at(tab_index).len() == 1 {
            return Err(LayoutError::LastPaneInTab {
                tab_id: self.tabs[tab_index].tab_id,
            });
        }

        let root = self.tabs[tab_index].root.clone();
        self.tabs[tab_index].root = root
            .remove_leaf(pane_id)
            .expect("a non-singleton tab retains a root after deleting one leaf");
        self.panes.remove(&pane_id);
        self.advance_revision();
        Ok(())
    }

    pub fn delete_tab(
        &mut self,
        requester: &[u8],
        base_revision: u64,
        tab_id: TabId,
    ) -> Result<(), LayoutError> {
        self.check_mutation(base_revision)?;
        self.require_member(requester)?;
        if self.tabs.len() == 1 {
            return Err(LayoutError::LastTab);
        }
        let tab_index = self
            .tabs
            .iter()
            .position(|tab| tab.tab_id == tab_id)
            .ok_or(LayoutError::UnknownTab { tab_id })?;
        let pane_ids = self.pane_ids_in_tab_at(tab_index);
        if pane_ids.iter().any(|pane_id| {
            self.panes
                .get(pane_id)
                .is_none_or(|pane| pane.host_peer_id != requester)
        }) {
            return Err(LayoutError::NotTabHost { tab_id });
        }

        self.tabs.remove(tab_index);
        for pane_id in pane_ids {
            self.panes.remove(&pane_id);
        }
        self.advance_revision();
        Ok(())
    }

    pub fn pane_ids_in_tab(&self, tab_id: TabId) -> Vec<PaneId> {
        self.tabs
            .iter()
            .find(|tab| tab.tab_id == tab_id)
            .map(|tab| tab.root.pane_ids())
            .unwrap_or_default()
    }

    fn check_mutation(&self, base_revision: u64) -> Result<(), LayoutError> {
        if base_revision != self.revision {
            return Err(LayoutError::StaleRevision {
                expected: self.revision,
                got: base_revision,
            });
        }
        self.revision
            .checked_add(1)
            .ok_or(LayoutError::RevisionExhausted)?;
        Ok(())
    }

    fn require_member(&self, peer_id: &[u8]) -> Result<(), LayoutError> {
        self.members
            .iter()
            .any(|member| member.peer_id == peer_id)
            .then_some(())
            .ok_or(LayoutError::NotMember)
    }

    fn next_id(&self, id: u64) -> Result<u64, LayoutError> {
        id.checked_add(1).ok_or(LayoutError::IdExhausted)
    }

    fn advance_revision(&mut self) {
        self.revision = self
            .revision
            .checked_add(1)
            .expect("check_mutation verified revision can advance");
    }

    fn tab_index_for_pane(&self, pane_id: PaneId) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.root.contains_leaf(pane_id))
    }

    fn pane_ids_in_tab_at(&self, tab_index: usize) -> Vec<PaneId> {
        self.tabs[tab_index].root.pane_ids()
    }
}

impl Node {
    fn contains_leaf(&self, wanted: PaneId) -> bool {
        match self {
            Self::Leaf { pane_id } => *pane_id == wanted,
            Self::Split { first, second, .. } => {
                first.contains_leaf(wanted) || second.contains_leaf(wanted)
            }
        }
    }

    fn leaf_depth(&self, wanted: PaneId) -> Option<usize> {
        match self {
            Self::Leaf { pane_id } => (*pane_id == wanted).then_some(0),
            Self::Split { first, second, .. } => first
                .leaf_depth(wanted)
                .or_else(|| second.leaf_depth(wanted))
                .map(|depth| depth + 1),
        }
    }

    fn pane_ids(&self) -> Vec<PaneId> {
        match self {
            Self::Leaf { pane_id } => vec![*pane_id],
            Self::Split { first, second, .. } => {
                let mut pane_ids = first.pane_ids();
                pane_ids.extend(second.pane_ids());
                pane_ids
            }
        }
    }

    fn replace_leaf(&mut self, wanted: PaneId, replacement: Node) -> bool {
        match self {
            Self::Leaf { pane_id } if *pane_id == wanted => {
                *self = replacement;
                true
            }
            Self::Leaf { .. } => false,
            Self::Split { first, second, .. } => {
                first.replace_leaf(wanted, replacement.clone())
                    || second.replace_leaf(wanted, replacement)
            }
        }
    }

    fn remove_leaf(self, wanted: PaneId) -> Option<Node> {
        match self {
            Self::Leaf { pane_id } => (pane_id != wanted).then_some(Self::Leaf { pane_id }),
            Self::Split {
                axis,
                first,
                second,
            } => {
                if first.contains_leaf(wanted) {
                    match first.remove_leaf(wanted) {
                        Some(first) => Some(Self::Split {
                            axis,
                            first: Box::new(first),
                            second,
                        }),
                        None => Some(*second),
                    }
                } else if second.contains_leaf(wanted) {
                    match second.remove_leaf(wanted) {
                        Some(second) => Some(Self::Split {
                            axis,
                            first,
                            second: Box::new(second),
                        }),
                        None => Some(*first),
                    }
                } else {
                    Some(Self::Split {
                        axis,
                        first,
                        second,
                    })
                }
            }
        }
    }
}

fn validate_grid(grid_rows: u16, grid_cols: u16) -> Result<(), LayoutError> {
    (grid_rows > 0 && grid_cols > 0)
        .then_some(())
        .ok_or(LayoutError::InvalidGrid)
}
