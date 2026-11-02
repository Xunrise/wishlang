package wish

import com.intellij.icons.AllIcons
import com.intellij.openapi.actionSystem.ActionUpdateThread
import com.intellij.openapi.actionSystem.AnActionEvent
import com.intellij.openapi.project.DumbAwareAction
import com.intellij.openapi.project.DumbAwareToggleAction
import com.intellij.diff.DiffContentFactory
import com.intellij.diff.DiffManager
import com.intellij.diff.requests.SimpleDiffRequest
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.components.service
import com.intellij.openapi.fileEditor.OpenFileDescriptor
import com.intellij.openapi.fileTypes.FileTypeManager
import com.intellij.openapi.project.DumbAware
import com.intellij.openapi.project.Project
import com.intellij.openapi.util.Disposer
import com.intellij.openapi.vcs.changes.Change
import com.intellij.openapi.vcs.changes.SimpleContentRevision
import com.intellij.openapi.vfs.LocalFileSystem
import com.intellij.testFramework.LightVirtualFile
import com.intellij.openapi.wm.ToolWindow
import com.intellij.openapi.wm.ToolWindowFactory
import com.intellij.ui.DoubleClickListener
import com.intellij.ui.content.ContentFactory
import com.intellij.ui.treeStructure.Tree
import com.intellij.util.ui.UIUtil
import com.intellij.vcsUtil.VcsUtil
import java.awt.event.MouseEvent
import java.io.File
import java.util.concurrent.CompletableFuture
import javax.swing.JEditorPane
import javax.swing.tree.DefaultMutableTreeNode

internal const val WISH_TOOL_WINDOW = "Wish"

class WishToolWindowFactory : ToolWindowFactory, DumbAware {
    override fun createToolWindowContent(project: Project, toolWindow: ToolWindow) {
        val contents = ContentFactory.getInstance()
        val log = WishLogPanel(project)
        Disposer.register(toolWindow.disposable, log)
        toolWindow.contentManager.addContent(contents.createContent(log, "Log", false))
        val ticket = TicketPanel(project)
        Disposer.register(toolWindow.disposable, ticket)
        toolWindow.contentManager.addContent(contents.createContent(ticket, "Ticket", false))
        toolWindow.setTitleActions(listOf(FulfilWishes(), BuildOnSave()))
    }
}

/** Runs a cycle now: the manual trigger, for when build on save is off (or to retry). */
class FulfilWishes : DumbAwareAction("Fulfil Wishes", "Generate what your edits wish for now, as a save would", AllIcons.Actions.Execute) {
    override fun actionPerformed(e: AnActionEvent) {
        e.project?.service<WishService>()?.fulfil()
    }
}

/** Whether saving a file starts a cycle. */
class BuildOnSave : DumbAwareToggleAction("Build on Save", "Start a cycle whenever you save; off: only Fulfil Wishes does", AllIcons.Actions.BuildAutoReloadChanges) {
    override fun isSelected(e: AnActionEvent) = e.project?.service<WishService>()?.buildOnSave ?: true
    override fun setSelected(e: AnActionEvent, state: Boolean) {
        e.project?.service<WishService>()?.buildOnSave = state
    }
    override fun getActionUpdateThread() = ActionUpdateThread.BGT
}

/** Runs [ok] on the EDT with the response; a failed request (no server, no ticket) goes to [failed]. */
internal fun <T> CompletableFuture<T>.onEdt(project: Project, failed: (Throwable) -> Unit = {}, ok: (T) -> Unit) {
    whenComplete { value, error ->
        ApplicationManager.getApplication().invokeLater({ if (error == null) ok(value) else failed(error) }, project.disposed)
    }
}

/** `.wish/` holds wish's own bookkeeping, which is noise in a list of changed files. */
internal fun isWishFile(path: String) = path.startsWith(".wish/")

internal fun visibleFiles(files: List<FileStat>) = files.filterNot { isWishFile(it.path) }

/** The files as VCS changes (via `wish/show`), so the platform's changes browser can group and diff them. */
internal fun fetchChanges(
    project: Project, files: List<FileStat>, sha: String, base: String?, beforeLabel: String, afterLabel: String,
): CompletableFuture<List<Change>> {
    val svc = project.service<WishService>()
    val changes = files.map { f ->
        val path = VcsUtil.getFilePath(File(svc.pathOf(f.path)), false)
        svc.show(sha, f.path, base)
            .thenApply<Change?> { r ->
                val before = r.before.takeIf { it.isNotEmpty() }?.let { SimpleContentRevision(it, path, beforeLabel) }
                val after = r.after.takeIf { it.isNotEmpty() }?.let { SimpleContentRevision(it, path, afterLabel) }
                if (before == null && after == null) null else Change(before, after)
            }
            .exceptionally { null }
    }
    return CompletableFuture.allOf(*changes.toTypedArray()).thenApply { changes.mapNotNull { it.join() } }
}

internal fun changeFor(changes: List<Change>, path: String): Change? =
    changes.firstOrNull { (it.afterRevision ?: it.beforeRevision)?.file?.path?.endsWith("/$path") == true }

internal fun showDiff(project: Project, change: Change, title: String, beforeTitle: String, afterTitle: String) {
    val contents = DiffContentFactory.getInstance()
    val type = FileTypeManager.getInstance().getFileTypeByFileName((change.afterRevision ?: change.beforeRevision)!!.file.name)
    DiffManager.getInstance().showDiff(
        project,
        SimpleDiffRequest(
            title,
            contents.create(project, change.beforeRevision?.content.orEmpty(), type),
            contents.create(project, change.afterRevision?.content.orEmpty(), type),
            beforeTitle, afterTitle,
        ),
    )
}

internal fun openItem(project: Project, item: TItem) = openFile(project, item.file, item.line)

/** Open a file wish names (relative to its project, or to [root]) in the editor, at a 1-based line. */
internal fun openFile(project: Project, rel: String, line: Int = 1, root: String? = null) {
    val path = root?.let { "${it.trimEnd('/')}/$rel" } ?: project.service<WishService>().pathOf(rel)
    val file = LocalFileSystem.getInstance().refreshAndFindFileByPath(path) ?: return
    OpenFileDescriptor(project, file, (line - 1).coerceAtLeast(0), 0).navigate(true)
}

/** Open a file as it is in journal snapshot [sha], read-only: for a ticket no checkout has. */
internal fun openSnapshot(project: Project, sha: String, rel: String, line: Int = 1, label: String) {
    project.service<WishService>().show(sha, rel).onEdt(project) { r ->
        val type = FileTypeManager.getInstance().getFileTypeByFileName(rel.substringAfterLast('/'))
        val file = LightVirtualFile("${rel.substringAfterLast('/')} ($label)", type, r.after).apply { isWritable = false }
        OpenFileDescriptor(project, file, (line - 1).coerceAtLeast(0), 0).navigate(true)
    }
}

internal fun Tree.onDoubleClick(handler: (Any?) -> Unit) {
    object : DoubleClickListener() {
        override fun onDoubleClick(event: MouseEvent): Boolean {
            handler((lastSelectedPathComponent as? DefaultMutableTreeNode)?.userObject)
            return true
        }
    }.installOn(this)
}

/** A read-only HTML view that follows the IDE's font and theme. */
internal fun htmlPane(): JEditorPane = JEditorPane("text/html", "").apply {
    isEditable = false
    isOpaque = false
    putClientProperty(JEditorPane.HONOR_DISPLAY_PROPERTIES, true)
    font = UIUtil.getLabelFont()
}
