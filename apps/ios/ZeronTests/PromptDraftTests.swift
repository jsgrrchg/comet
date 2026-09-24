import XCTest
@testable import Zeron

final class PromptDraftTests: XCTestCase {
    @MainActor func testDraftsKeepConcurrentContentAndMoveWithoutChangingIt() {
        let doc = RegistryDoc(deviceId: "phone")
        let content = PromptDraftContent(prompt: "Keep this", target: PromptDraftTarget(deviceId: "host"))
        let a = PromptDraftSave(id: "a", revision: "v1", createdAt: 1, content: content)
        doc.publishPromptDraft(a)
        doc.publishPromptDraft(PromptDraftSave(id: "b", revision: "v2", createdAt: 2, content: content))
        doc.publishPromptDraft(PromptDraftSave(id: "a", revision: "v3", baseRevision: "v1", createdAt: 1, content: content))
        doc.publishPromptDraft(PromptDraftSave(id: "a", revision: "v4", baseRevision: "v1", createdAt: 1, content: content))
        XCTAssertEqual(doc.promptDraftRows.count, 3)
        XCTAssertTrue(doc.promptDraftRows.contains { $0.id == "v3" && $0.conflict })
        doc.movePromptDraft("a", before: "b", after: nil)
        XCTAssertEqual(doc.promptDraftRows.first?.id, "a")
        XCTAssertEqual(doc.promptDraftRows.first?.revision, "v4")
        doc.write(kind: "promptDrafts", id: "a", op: .upsert, set: ["closed": .bool(true)])
        doc.movePromptDraft("a", before: "b", after: nil)
        XCTAssertFalse(doc.promptDraftRows.contains { $0.id == "a" })
    }
}
