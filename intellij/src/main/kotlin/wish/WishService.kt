package wish

import com.intellij.codeInsight.daemon.DaemonCodeAnalyzer
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

fun uriOf(file: VirtualFile): String = File(file.path).toPath().toUri().toString()

/**
 * The `wish` binary: `$WISH_BIN`, else `~/.cargo/bin/wish`. PATH is deliberately not searched,
 * because `/usr/bin/wish` on macOS is an unrelated Tk shell.
 */
fun wishBinary(env: Map<String, String> = System.getenv(), home: String = System.getProperty("user.home")): File =
    File(env["WISH_BIN"]?.takeIf { it.isNotBlank() } ?: "$home/.cargo/bin/wish")

/** One `wish lsp` per project; everything the editor and the tool window show comes from it. */
@Service(Service.Level.PROJECT)
class WishService(private val project: Project) : Disposable {
    /** Latest `wish/fileState` per file URI. */
    val states = ConcurrentHashMap<String, FileState>()

    private val versions = ConcurrentHashMap<String, Int>()
    private var process: Process? = null
    @Volatile private var server: WishServer? = null

    private val refetchQueue = MergingUpdateQueue("wish refetch", 200, true, null, this)

    suspend fun start() {
        val bin = wishBinary()
        try {
            val s = withContext(Dispatchers.IO) { launch(bin) }
            server = s
            s.initialized(InitializedParams())
        } catch (e: Exception) {
            thisLogger().warn("wish lsp failed to start ($bin)", e)
            notify(
                MessageType.Error,
                "Could not start `$bin lsp`: ${e.message}. Set \$WISH_BIN or install wish to ~/.cargo/bin/wish; " +
                    "PATH is not searched because /usr/bin/wish is the Tk shell.",
            )
            return
        }
        listen()
        FileEditorManager.getInstance(project).openFiles.forEach(::open)
        publishChanged()
    }

    private suspend fun launch(bin: File): WishServer {
        val dir = File(project.basePath ?: ".")
        val p = ProcessBuilder(bin.path, "lsp").directory(dir).redirectError(ProcessBuilder.Redirect.DISCARD).start()
        process = p
        val launcher = Launcher.createLauncher(Client(), WishServer::class.java, p.inputStream, p.outputStream)
        launcher.startListening()
        val params = InitializeParams().apply {
            rootUri = dir.toPath().toUri().toString().trimEnd('/')
            processId = ProcessHandle.current().pid().toInt()
            capabilities = ClientCapabilities()
        }
        launcher.remoteProxy.initialize(params).await()
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
                        server?.textDocumentService?.didSave(DidSaveTextDocumentParams(TextDocumentIdentifier(uri)))
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
        val document = FileDocumentManager.getInstance().getDocument(file) ?: return
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
        val file = LocalFileSystem.getInstance().findFileByPath(File(java.net.URI(uri)).path) ?: return
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

    fun log(): CompletableFuture<LogResult> = request { it.log() }

    fun ticket(): CompletableFuture<TicketResult> = request { it.ticket() }

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
            val path = File(java.net.URI(p.uri)).path
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
    }
}

class WishStartup : ProjectActivity {
    override suspend fun execute(project: Project) {
        if (ApplicationManager.getApplication().isUnitTestMode) return
        project.service<WishService>().start()
    }
}
