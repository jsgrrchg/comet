use super::*;
use zeron_proto::{SidebarPinChange, pin_order_key_between, valid_pin_order_key};

impl RegistryDoc {
    /// Personal-cut upgrade marker, replicated with this user/org registry.
    /// Kept even when every pin has subsequently been removed.
    pub fn sidebar_pin_migration_complete(&self) -> bool {
        self.overlay_row(KIND_PREFERENCES, SIDEBAR_PINS_STATE_ID)
            .is_some_and(|row| row.fields.get("personalCutLegacyImported") == Some(&json!(true)))
    }

    fn finish_sidebar_pin_migration(&mut self) {
        if !self.sidebar_pin_migration_complete() {
            self.write(
                KIND_PREFERENCES,
                SIDEBAR_PINS_STATE_ID,
                OpKind::Upsert,
                fields([("personalCutLegacyImported", json!(true))]),
            );
        }
    }

    /// Import the old registry list, or the profile's local list if no registry
    /// list exists. Build all operations first and enqueue pins + marker in one
    /// batch. The legacy source is deliberately retained as a backup.
    pub fn migrate_sidebar_pins(
        &mut self,
        authoritative: bool,
        local: Option<&[String]>,
    ) -> Result<bool, DocError> {
        if !authoritative || self.sidebar_pin_migration_complete() {
            return Ok(false);
        }
        let state = self.overlay_row(KIND_PREFERENCES, SIDEBAR_PINS_STATE_ID);
        // Respect a new-format installation, including its explicitly empty
        // state and unpin tombstones. Only our own new placeholder may import.
        if !self.overlay_rows(KIND_SIDEBAR_PINS).is_empty()
            || state
                .is_some_and(|row| row.fields.get("personalCutLegacyPending") != Some(&json!(true)))
        {
            self.finish_sidebar_pin_migration();
            return Ok(true);
        }
        let legacy = self.overlay_row(KIND_PREFERENCES, "sidebar-v1");
        let source: Vec<String> = if let Some(row) = &legacy {
            serde_json::from_value(
                row.fields
                    .get("pinnedSessionIds")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            )
            .map_err(|_| DocError::Schema("Invalid legacy sidebar pins; source retained".into()))?
        } else if let Some(local) = local {
            local.to_vec()
        } else {
            return Ok(false);
        };
        if source.len() > MAX_SIDEBAR_PINS {
            return Err(DocError::Schema(
                "Legacy sidebar pin limit exceeded; source retained".into(),
            ));
        }
        let known: std::collections::HashSet<_> =
            self.read_chats()?.into_iter().map(|c| c.id).collect();
        let mut seen = std::collections::HashSet::new();
        let ids: Vec<_> = source
            .into_iter()
            .filter(|id| known.contains(id) && seen.insert(id.clone()))
            .collect();
        if ids.iter().any(|id| !valid_session_id(id)) {
            return Err(DocError::Schema(
                "Invalid legacy pinned session ID; source retained".into(),
            ));
        }
        // Stable legacy clocks and keys make simultaneous registry migrations
        // identical and prevent a delayed import from beating a later unpin.
        let hlc = legacy
            .as_ref()
            .and_then(|row| row.max_clock())
            .map(str::to_owned)
            // Device-local lists have no shared field clock. Treat them as
            // legacy seeds, older than any user edit on every device.
            .unwrap_or_else(|| encode_hlc(0, 0, "personal-cut-legacy"));
        let mut ops: Vec<_> = ids
            .into_iter()
            .enumerate()
            .map(|(index, id)| RowOp {
                kind: KIND_SIDEBAR_PINS.into(),
                id,
                op: OpKind::Upsert,
                set: Some(fields([
                    ("pinned", json!(true)),
                    ("orderKey", json!(format!("{index:08x}8"))),
                ])),
                hlc: hlc.clone(),
                clocks: None,
            })
            .collect();
        ops.push(RowOp {
            kind: KIND_PREFERENCES.into(),
            id: SIDEBAR_PINS_STATE_ID.into(),
            op: OpKind::Upsert,
            set: Some(fields([
                ("initialized", json!(true)),
                ("personalCutLegacyImported", json!(true)),
            ])),
            hlc: self.next_hlc(),
            clocks: None,
        });
        self.enqueue_ops(ops);
        Ok(true)
    }

    pub fn sidebar_pins_initialized(&self) -> bool {
        self.overlay_row(KIND_PREFERENCES, SIDEBAR_PINS_STATE_ID)
            .is_some()
    }

    pub(super) fn ordered_sidebar_pins(&self) -> Vec<(String, String)> {
        let mut pins: Vec<_> = self
            .overlay_rows(KIND_SIDEBAR_PINS)
            .into_iter()
            .filter_map(|row| {
                if row.fields.get("pinned")?.as_bool()? != true {
                    return None;
                }
                let key = row.fields.get("orderKey")?.as_str()?;
                valid_pin_order_key(key).then(|| (row.id, key.to_owned()))
            })
            .collect();
        pins.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        pins
    }

    /// Readiness survives restarts even when the list is empty. No data import.
    pub(super) fn initialize_sidebar_pins(&mut self) {
        if !self.sidebar_pins_initialized() {
            self.write(
                KIND_PREFERENCES,
                SIDEBAR_PINS_STATE_ID,
                OpKind::Upsert,
                fields([
                    ("initialized", json!(true)),
                    ("personalCutLegacyPending", json!(true)),
                ]),
            );
        }
    }

    pub fn change_sidebar_pin(&mut self, change: &SidebarPinChange) -> Result<(), DocError> {
        let id = change.session_id();
        if !valid_session_id(id) {
            return Err(DocError::Schema("Invalid pinned session ID".into()));
        }
        let current = self
            .sidebar_preferences()
            .unwrap_or_default()
            .pinned_session_ids;
        if matches!(change, SidebarPinChange::Pin { .. })
            && !current.iter().any(|v| v == id)
            && current.len() >= MAX_SIDEBAR_PINS
        {
            return Err(DocError::Schema("You can pin up to 200 sessions".into()));
        }
        if matches!(change, SidebarPinChange::Pin { .. })
            && self.overlay_row(KIND_CHATS, id).is_none()
        {
            return Err(DocError::Schema("Session no longer exists".into()));
        }
        self.initialize_sidebar_pins();
        self.finish_sidebar_pin_migration();
        // Observe this pin's field clocks before a causally subsequent edit,
        // including clocks from a device whose wall clock runs ahead of ours.
        if let Some(row) = self.overlay_row(KIND_SIDEBAR_PINS, id) {
            if let Some(clock) = row.max_clock() {
                let mut parts = clock.splitn(3, '-');
                if let (Some(ms), Some(counter)) = (
                    parts.next().and_then(|v| v.parse::<i64>().ok()),
                    parts.next().and_then(|v| v.parse::<u32>().ok()),
                ) {
                    if (ms, counter) > (self.clock.last_ms, self.clock.counter) {
                        self.clock.last_ms = ms;
                        self.clock.counter = counter;
                    }
                }
            }
        }
        if matches!(change, SidebarPinChange::Unpin { .. }) {
            self.write(
                KIND_SIDEBAR_PINS,
                id,
                OpKind::Upsert,
                fields([("pinned", json!(false))]),
            );
            return Ok(());
        }
        if matches!(change, SidebarPinChange::Move { .. }) && !current.iter().any(|v| v == id) {
            return Ok(());
        }
        let pins = self.ordered_sidebar_pins();
        let mut next = current.clone();
        change.project(&mut next);
        let index = next.iter().position(|v| v == id).unwrap();
        let key_for = |id: &String| {
            pins.iter()
                .find(|(pin, _)| pin == id)
                .map(|(_, key)| key.as_str())
        };
        let lower = index
            .checked_sub(1)
            .and_then(|i| next.get(i))
            .and_then(key_for);
        let upper = next.get(index + 1).and_then(key_for);
        let hlc = self.next_hlc();
        let key =
            pin_order_key_between(lower, upper, &hlc).map_err(|e| DocError::Schema(e.into()))?;
        let mut set = fields([("orderKey", json!(key))]);
        if matches!(change, SidebarPinChange::Pin { .. }) {
            set.insert("pinned".into(), json!(true));
        }
        self.enqueue_ops(vec![RowOp {
            kind: KIND_SIDEBAR_PINS.into(),
            id: id.into(),
            op: OpKind::Upsert,
            set: Some(set),
            hlc,
            clocks: None,
        }]);
        Ok(())
    }
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:@/-".contains(&b))
}
