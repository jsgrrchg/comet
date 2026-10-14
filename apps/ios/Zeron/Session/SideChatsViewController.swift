import UIKit

/// One session's side chats — forked from it, or spawned by its agent through
/// the Zeron MCP server — newest first. Shown as a sheet over the session;
/// picking one opens it on top of the session it hangs off.
final class SideChatsViewController: SessionListController {
    private let parentId: String
    private let onOpen: (String) -> Void
    private var childrenToken: AnyObject?

    init(app: AppModel, parentId: String, onOpen: @escaping (String) -> Void) {
        self.parentId = parentId
        self.onOpen = onOpen
        super.init(app: app)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        sideRows = true
        super.viewDidLoad()
        title = "Side Chats"
        navigationItem.largeTitleDisplayMode = .never
        navigationItem.rightBarButtonItem = UIBarButtonItem(systemItem: .done, primaryAction: UIAction { [weak self] _ in
            self?.dismiss(animated: true)
        })
        collectionView.accessibilityIdentifier = "side-chats-list"
        // Children aren't on the front page: follow every workspace revision.
        childrenToken = app.observeWorkspace { [weak self] in
            guard let self else { return }
            self.reload(animated: true)
            if self.sessions.isEmpty { self.dismiss(animated: true) }
        }
    }

    override func buildSections() -> [(id: String, header: String?, folders: [FolderRowVM], sessions: [SessionRowVM])] {
        [("children", nil, [], app.children(of: parentId))]
    }

    override func openSession(_ id: String) {
        onOpen(id)
    }
}
