package wish

import com.intellij.codeInsight.daemon.DaemonCodeAnalyzer
import com.intellij.ide.util.PropertiesComponent
import com.intellij.notification.Notification
import com.intellij.notification.NotificationAction
import com.intellij.notification.NotificationGroupManager
import com.intellij.notification.NotificationType
import com.intellij.openapi.Disposable
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.application.ReadAction
import com.intellij.openapi.components.Service
import com.intellij.openapi.components.service
import com.intellij.openapi.diagnostic.thisLogger
import com.intellij.openapi.editor.Document
import com.intellij.openapi.editor.event.DocumentEvent
import com.intellij.openapi.editor.event.DocumentListener
import com.intellij.openapi.editor.EditorFactory
import com.intellij.openapi.fileEditor.FileDocumentManager
import com.intellij.openapi.fileEditor.FileEditorManager
import com.intellij.openapi.fileEditor.FileEditorManagerListener
import com.intellij.openapi.fileEditor.OpenFileDescriptor
import com.intellij.openapi.project.Project
import com.intellij.openapi.startup.ProjectActivity
import com.intellij.openapi.vfs.LocalFileSystem
import com.intellij.openapi.vfs.VirtualFile
import com.intellij.openapi.vfs.VirtualFileManager
import com.intellij.openapi.vfs.newvfs.BulkFileListener
import com.intellij.openapi.vfs.newvfs.events.VFileContentChangeEvent
import com.intellij.openapi.vfs.newvfs.events.VFileEvent
import com.intellij.openapi.wm.WindowManager
import com.intellij.psi.PsiManager
import com.intellij.util.messages.Topic
import com.intellij.util.ui.update.MergingUpdateQueue
import com.intellij.util.ui.update.Update
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.future.await
import kotlinx.coroutines.withContext
import org.eclipse.lsp4j.ClientCapabilities
import org.eclipse.lsp4j.DidChangeTextDocumentParams
import org.eclipse.lsp4j.DidOpenTextDocumentParams
import org.eclipse.lsp4j.DidSaveTextDocumentParams
import org.eclipse.lsp4j.ExecuteCommandParams
import org.eclipse.lsp4j.InitializeParams
import org.eclipse.lsp4j.InitializedParams
import org.eclipse.lsp4j.MessageActionItem
import org.eclipse.lsp4j.MessageParams
import org.eclipse.lsp4j.MessageType
import org.eclipse.lsp4j.ProgressParams
import org.eclipse.lsp4j.PublishDiagnosticsParams
import org.eclipse.lsp4j.ShowDocumentParams
import org.eclipse.lsp4j.ShowDocumentResult
import org.eclipse.lsp4j.ShowMessageRequestParams
import org.eclipse.lsp4j.TextDocumentContentChangeEvent
import org.eclipse.lsp4j.TextDocumentIdentifier
import org.eclipse.lsp4j.TextDocumentItem
import org.eclipse.lsp4j.VersionedTextDocumentIdentifier
import org.eclipse.lsp4j.WorkDoneProgressBegin
import org.eclipse.lsp4j.WorkDoneProgressCreateParams
import org.eclipse.lsp4j.WorkDoneProgressEnd
import org.eclipse.lsp4j.WorkDoneProgressReport
import org.eclipse.lsp4j.jsonrpc.Launcher
import java.io.File
import java.util.concurrent.CompletableFuture
import java.util.concurrent.ConcurrentHashMap

/** Published on the project bus whenever wish's journal, tickets or file states may have changed. */
fun interface WishListener {
    fun changed()
}

/** The URI `wish lsp` knows [file] by (a Linux path when the project lives in WSL, see [Bridge]). */
fun uriOf(file: VirtualFile): String = serverUri(file.path)

/**
 * The `wishlang` binary: `$WISH_BIN`, else the first of the usual install locations that exists
 * (the old `~/.cargo/bin/wish` last). PATH is deliberately not searched for `wish`, because
 * `/usr/bin/wish` on macOS is an unrelated Tk shell.
 */
fun wishBinary(env: Map<String, String> = System.getenv(), home: String = System.getProperty("user.home")): File {
    env["WISH_BIN"]?.takeIf { it.isNotBlank() }?.let { return File(it) }
    val candidates = listOf("$home/.local/bin/wishlang", "/opt/homebrew/bin/wishlang", "/usr/local/bin/wishlang", "$home/.cargo/bin/wishlang", "$home/.cargo/bin/wish")
    return candidates.map(::File).firstOrNull { it.canExecute() } ?: File(candidates[0])
}

/** One `wish lsp` per project; everything the editor and the tool window show comes from it. */
@Service(Service.Level.PROJECT)
class WishService(private val project: Project) : Disposable {
    /** Latest `wish/fileState` per file URI. */
    val states = ConcurrentHashMap<String, FileState>()

    private val versions = ConcurrentHashMap<String, Int>()
    private val bridge = Bridge.of(project.basePath)

    /** The wish project's directory (IDE path): `wish.toml` may sit below the IDE project, e.g. in `backend/`. */
    @Volatile private var root: String? = null

    /** The IDE path of a file wish names relative to its project. */
    fun pathOf(rel: String): String = "${(root ?: project.basePath ?: ".").trimEnd('/')}/$rel"

    private fun remember(uri: String?) {
        uri?.let { root = bridge.path(it).trimEnd('/') }
    }

    /** Start a cycle on every save (the default), or only through [fulfil]. Kept per project. */
    var buildOnSave: Boolean
        get() = PropertiesComponent.getInstance(project).getBoolean(BUILD_ON_SAVE, true)
        set(value) = PropertiesComponent.getInstance(project).setValue(BUILD_ON_SAVE, value, true)

    /** Run a cycle for your edits now, as a save would. */
    fun fulfil(): CompletableFuture<Any?> = execute("wish.cycle")
    private var process: Process? = null
    @Volatile private var server: WishServer? = null

    private val refetchQueue = MergingUpdateQueue("wish refetch", 200, true, null, this)

    suspend fun start() {
        val root = project.basePath ?: "."
        val bin = bridge.command(root).joinToString(" ")
        try {
            val s = withContext(Dispatchers.IO) { launch(root) }
            server = s
            s.initialized(InitializedParams())
        } catch (e: Exception) {
            thisLogger().warn("wish lsp failed to start ($bin)", e)
            notify(
                MessageType.Error,
                if (bridge is Bridge.Wsl) {
                    "Could not start wish in WSL (${bridge.distro}): ${e.message}. Install it there with " +
                        "`cargo install --path wish` (~/.cargo/bin/wishlang), or set \$WISH_BIN in its ~/.bashrc."
                } else {
                    "Could not start `$bin`: ${e.message}. Set \$WISH_BIN or install wishlang (~/.local/bin, Homebrew or ~/.cargo/bin); " +
                        "PATH is not searched because /usr/bin/wish is the Tk shell."
                },
            )
            return
        }
        listen()
        FileEditorManager.getInstance(project).openFiles.forEach(::open)
        publishChanged()
    }

    private suspend fun launch(root: String): WishServer {
        val p = ProcessBuilder(bridge.command(root)).directory(bridge.directory(root))
            .redirectError(ProcessBuilder.Redirect.DISCARD).start()
        process = p
        val launcher = Launcher.createLauncher(Client(), WishServer::class.java, p.inputStream, p.outputStream)
        launcher.startListening()
        val params = InitializeParams().apply {
            rootUri = bridge.uri(root).trimEnd('/')
            processId = ProcessHandle.current().pid().toInt()
            capabilities = ClientCapabilities()
        }
        // Through wsl.exe a missing binary is not a start failure but an early exit (127: not found).
        val init = launcher.remoteProxy.initialize(params)
        p.onExit().thenRun { init.completeExceptionally(IllegalStateException("wish lsp exited with code ${p.exitValue()}")) }
        init.await()
        return launcher.remoteProxy
    }

    private fun listen() {
        project.messageBus.connect(this).subscribe(FileEditorManagerListener.FILE_EDITOR_MANAGER, object : FileEditorManagerListener {
            override fun fileOpened(source: FileEditorManager, file: VirtualFile) = open(file)
        })
        EditorFactory.getInstance().eventMulticaster.addDocumentListener(object : DocumentListener {
            override fun documentChanged(event: DocumentEvent) {
                val file = FileDocumentManager.getInstance().getFile(event.document) ?: return
                if (isJava(file)) change(file, event.document)
            }
        }, this)
        // A save is only visible to the server once the bytes are on disk, which is what this event reports.
        ApplicationManager.getApplication().messageBus.connect(this).subscribe(VirtualFileManager.VFS_CHANGES, object : BulkFileListener {
            override fun after(events: List<VFileEvent>) {
                events.filterIsInstance<VFileContentChangeEvent>().filter { it.isFromSave }.forEach {
                    val uri = uriOf(it.file)
                    if (versions.containsKey(uri)) {
                        // A save is what starts a cycle; with build on save off, only Fulfil Wishes does.
                        if (buildOnSave) server?.textDocumentService?.didSave(DidSaveTextDocumentParams(TextDocumentIdentifier(uri)))
                        refetch()
                    }
                }
            }
        })
    }

    private fun isJava(file: VirtualFile) = file.extension == "java"

    private fun textOf(document: Document): String = ReadAction.compute<String, RuntimeException> { document.text }

    private fun open(file: VirtualFile) {
        if (!isJava(file)) return
        // start() calls this from a background coroutine, where model reads need a read action.
        val document = ReadAction.compute<Document?, RuntimeException> {
            FileDocumentManager.getInstance().getDocument(file)
        } ?: return
        val uri = uriOf(file)
        versions[uri] = 1
        server?.textDocumentService?.didOpen(DidOpenTextDocumentParams(TextDocumentItem(uri, "java", 1, textOf(document))))
        fetch(uri)
    }

    private fun change(file: VirtualFile, document: Document) {
        val uri = uriOf(file)
        val version = versions.computeIfPresent(uri) { _, v -> v + 1 } ?: return
        server?.textDocumentService?.didChange(
            DidChangeTextDocumentParams(VersionedTextDocumentIdentifier(uri, version), listOf(TextDocumentContentChangeEvent(textOf(document)))),
        )
    }

    private fun fetch(uri: String) {
        val s = server ?: return
        s.fileState(FileStateParams(TextDocumentIdentifier(uri))).thenAccept { state ->
            states[uri] = state
            ApplicationManager.getApplication().invokeLater({ refresh(uri, state) }, project.disposed)
        }
    }

    /** Repaint the bars of the file's editors and re-run highlighting for that file only. */
    private fun refresh(uri: String, state: FileState) {
        val file = LocalFileSystem.getInstance().findFileByPath(bridge.path(uri)) ?: return
        paintBars(project, file, state)
        PsiManager.getInstance(project).findFile(file)?.let { DaemonCodeAnalyzer.getInstance(project).restart(it) }
    }

    /** Re-fetch every open file, at most once per burst of changes. */
    private fun refetch() {
        refetchQueue.queue(object : Update("refetch") {
            override fun run() = versions.keys.forEach(::fetch)
        })
    }

    private fun publishChanged() = project.messageBus.syncPublisher(CHANGED).changed()

    private fun <T> request(call: (WishServer) -> CompletableFuture<T>): CompletableFuture<T> =
        server?.let(call) ?: CompletableFuture.failedFuture(IllegalStateException("wish lsp is not running"))

    fun log(): CompletableFuture<LogResult> = request { it.log() }.thenApply { r -> remember(r.root); r }

    /** The ticket of this branch, or of another ticket [branch] (which leaves this project's root alone). */
    fun ticket(branch: String? = null): CompletableFuture<TicketResult> =
        request { it.ticket(TicketParams(branch)) }.thenApply { r -> if (branch == null) remember(r.root); r }

    fun tickets(): CompletableFuture<List<TicketRef>> = request { it.tickets() }

    /** The IDE path of a server URI (a Linux path mapped to `\\wsl.localhost\…` when the project is in WSL). */
    fun localPath(uri: String): String = bridge.path(uri).trimEnd('/')

    fun show(sha: String, path: String, base: String? = null): CompletableFuture<ShowResult> =
        request { it.show(ShowParams(sha, path, base)) }

    /** Completes once the server has run the command, so callers can reload then. */
    fun execute(command: String, arguments: List<Any> = emptyList()): CompletableFuture<Any?> =
        request { it.workspaceService.executeCommand(ExecuteCommandParams(command, arguments)) }

    fun notify(type: MessageType, message: String): Notification {
        val kind = when (type) {
            MessageType.Error -> NotificationType.ERROR
            MessageType.Warning -> NotificationType.WARNING
            else -> NotificationType.INFORMATION
        }
        return NotificationGroupManager.getInstance().getNotificationGroup("Wish").createNotification("Wish", message, kind).also { it.notify(project) }
    }

    private fun status(text: String?) = ApplicationManager.getApplication().invokeLater {
        WindowManager.getInstance().getStatusBar(project)?.info = text.orEmpty()
    }

    private inner class Client : WishClient {
        private val done = CompletableFuture.completedFuture<Void?>(null)

        override fun changed(params: Any?) {
            refetch()
            publishChanged()
        }

        override fun telemetryEvent(o: Any?) {}
        override fun publishDiagnostics(d: PublishDiagnosticsParams?) {}
        override fun logMessage(m: MessageParams?) {}
        override fun showMessage(m: MessageParams) {
            notify(m.type, m.message)
        }

        override fun showMessageRequest(p: ShowMessageRequestParams): CompletableFuture<MessageActionItem?> {
            val answer = CompletableFuture<MessageActionItem?>()
            val n = notify(p.type, p.message)
            p.actions?.forEach { item -> n.addAction(NotificationAction.createSimple(item.title) { answer.complete(item); n.expire() }) }
            n.whenExpired { answer.complete(null) }
            return answer
        }

        override fun showDocument(p: ShowDocumentParams): CompletableFuture<ShowDocumentResult> {
            val path = bridge.path(p.uri)
            ApplicationManager.getApplication().invokeLater({
                LocalFileSystem.getInstance().refreshAndFindFileByPath(path)?.let {
                    val at = p.selection?.start
                    OpenFileDescriptor(project, it, at?.line ?: 0, at?.character ?: 0).navigate(true)
                }
            }, project.disposed)
            return CompletableFuture.completedFuture(ShowDocumentResult(true))
        }

        override fun createProgress(p: WorkDoneProgressCreateParams?) = done
        override fun notifyProgress(p: ProgressParams) {
            when (val v = p.value.left) {
                is WorkDoneProgressBegin -> status("Wish: ${v.title} ${v.message.orEmpty()}")
                is WorkDoneProgressReport -> status("Wish: ${v.message.orEmpty()}")
                is WorkDoneProgressEnd -> status(v.message?.let { "Wish: $it" })
            }
        }

        override fun refreshSemanticTokens() = done
        override fun refreshCodeLenses() = done
        override fun refreshInlayHints() = done
        override fun refreshDiagnostics() = done
    }

    override fun dispose() {
        process?.destroy()
    }

    companion object {
        @JvmField
        val CHANGED: Topic<WishListener> = Topic.create("Wish changed", WishListener::class.java)

        private const val BUILD_ON_SAVE = "wish.buildOnSave"
    }
}

class WishStartup : ProjectActivity {
    override suspend fun execute(project: Project) {
        if (ApplicationManager.getApplication().isUnitTestMode) return
        project.service<WishService>().start()
    }
}
