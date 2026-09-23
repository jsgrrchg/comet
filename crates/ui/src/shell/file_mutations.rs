//! Coordinate structural changes across every editor of the affected workspace.
use super::*;
use crate::files::{client::WorkspaceFilesClient, mutations::MutationIntent};
use zeron_proto::{
    DeleteWorkspaceEntryRequest, MoveWorkspaceEntryRequest, WorkspaceMutationOutcome,
};

impl Shell {
    pub(super) fn start_file_mutation(
        &mut self,
        source: Entity<FilesSurface>,
        mut intent: MutationIntent,
        cx: &mut Context<Self>,
    ) {
        if !source.read(cx).accepts_origin(&intent.origin, cx) {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let surfaces = self
            .files
            .values()
            .chain(self.file_surfaces.values())
            .filter(|surface| surface.read(cx).shares_workspace(&intent.origin))
            .cloned()
            .collect::<Vec<_>>();
        if surfaces
            .iter()
            .any(|surface| surface.read(cx).mutation_busy())
        {
            source.update(cx, |files, cx| {
                files.report_mutation_error("Another file operation is still running".into(), cx)
            });
            return;
        }
        for surface in &surfaces {
            surface.update(cx, |files, cx| files.prepare_mutation(intent.clone(), cx));
        }
        let waited_for_save = surfaces
            .iter()
            .any(|surface| surface.read(cx).mutation_has_save());
        let client = WorkspaceFilesClient::new(engine, intent.origin.context.clone());
        cx.spawn(async move |_, cx| {
            let result: Result<WorkspaceMutationOutcome, String> = async {
                let mut ready = false;
                for _ in 0..400 {
                    if !surfaces
                        .iter()
                        .any(|surface| surface.read_with(cx, |files, _| files.mutation_has_save()))
                    {
                        ready = true;
                        break;
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(25))
                        .await;
                }
                if !ready {
                    return Err("Wait for the pending save to finish and try again".into());
                }
                if !source.read_with(cx, |files, cx| files.accepts_origin(&intent.origin, cx)) {
                    return Err("Workspace changed before the operation started".into());
                }
                // Only refresh the source revision when our own save was awaited.
                // Otherwise an external edit must cause SourceChanged, not consent.
                if waited_for_save {
                    let page = client
                        .list_directory_snapshot(
                            zeron_proto::ListWorkspaceDirectoryRequest {
                                target: intent.origin.context.target.clone(),
                                directory: crate::files::model::parent_path(&intent.entry.path)
                                    .unwrap_or_default(),
                                include_ignored: true,
                                cursor: None,
                            },
                            &[intent.entry.path.clone()],
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                    if page.checkout_id != intent.origin.checkout_id {
                        return Err("Workspace changed while saving".into());
                    }
                    let entry = page
                        .entries
                        .into_iter()
                        .find(|entry| entry.path == intent.entry.path)
                        .ok_or("Source no longer exists")?;
                    intent.entry.mutation_revision = entry.mutation_revision;
                }
                let checkout = intent
                    .origin
                    .checkout_id
                    .clone()
                    .ok_or("Workspace identity unavailable")?;
                let revision = intent
                    .entry
                    .mutation_revision
                    .clone()
                    .ok_or("Refresh the tree before trying again")?;
                if let Some(destination) = &intent.destination {
                    client
                        .move_entry(MoveWorkspaceEntryRequest {
                            target: intent.origin.context.target.clone(),
                            operation_id: intent.operation_id.clone(),
                            expected_checkout_id: checkout,
                            source_path: intent.entry.path.clone(),
                            destination_path: destination.clone(),
                            expected_source_revision: revision,
                            expected_kind: intent.entry.kind,
                        })
                        .await
                        .map_err(|e| e.to_string())
                } else {
                    client
                        .delete_entry(DeleteWorkspaceEntryRequest {
                            target: intent.origin.context.target.clone(),
                            operation_id: intent.operation_id.clone(),
                            expected_checkout_id: checkout,
                            path: intent.entry.path.clone(),
                            expected_source_revision: revision,
                            expected_kind: intent.entry.kind,
                            recursive: intent.entry.kind
                                == zeron_proto::WorkspaceEntryKind::Directory,
                        })
                        .await
                        .map_err(|e| e.to_string())
                }
            }
            .await;
            for surface in &surfaces {
                let _ = surface.update(cx, |files, cx| files.finish_mutation(&intent, &result, cx));
            }
        })
        .detach();
    }
}
