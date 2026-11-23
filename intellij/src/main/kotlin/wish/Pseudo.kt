package wish

import com.google.gson.Gson
import com.intellij.notification.NotificationGroupManager
import com.intellij.notification.NotificationType
import com.intellij.openapi.Disposable
import com.intellij.openapi.actionSystem.ActionUpdateThread
import com.intellij.openapi.actionSystem.AnActionEvent
import com.intellij.openapi.actionSystem.CommonDataKeys
import com.intellij.openapi.actionSystem.ToggleAction
import com.intellij.openapi.application.ApplicationManager
import com.intellij.openapi.editor.Editor
import com.intellij.openapi.editor.FoldRegion
import com.intellij.openapi.editor.impl.EditorImpl
import com.intellij.openapi.editor.event.DocumentEvent
import com.intellij.openapi.editor.event.DocumentListener
import com.intellij.openapi.progress.ProgressIndicator
import com.intellij.openapi.progress.Task
import com.intellij.openapi.project.DumbAware
import com.intellij.openapi.project.Project
import com.intellij.openapi.util.Disposer
import com.intellij.openapi.util.Key
import com.intellij.util.EnvironmentUtil

/** `wishlang pseudo --json`: per method its outline, body range and step folds, in Document offsets. */
data class PseudoFile(val methods: List<PseudoMethod> = emptyList())
data class PseudoMethod(val path: String = "", val outline: String = "", val body: List<Int> = emptyList(), val folds: List<PseudoFold> = emptyList(), val fallback: String? = null)
data class PseudoFold(val start: Int = 0, val end: Int = 0, val placeholder: String = "")

/** A fold to add: outline folds hold the step folds of their method. */
data class PseudoRegion(val start: Int, val end: Int, val placeholder: String, val expanded: Boolean)

/**
 * The regions for a file: each method body folds to `{ outline }`, expanded when it has steps so the
 * steps show; each step folds to its line (the CLI already indented it).
 */
fun pseudoRegions(file: PseudoFile): List<PseudoRegion> = file.methods.filter { it.body.size == 2 }.flatMap { m ->
    listOf(PseudoRegion(m.body[0], m.body[1], "{ ${m.outline} }", expanded = m.folds.isNotEmpty())) +
        m.folds.map { PseudoRegion(it.start, it.end, it.placeholder, expanded = false) }
}

private val PSEUDO = Key.create<Pseudo>("wish.pseudo")

/** The pseudo folds of one editor, gone on toggle-off or on the first edit (their offsets would be stale). */
private class Pseudo(val editor: Editor) : Disposable {
    var regions: List<FoldRegion> = emptyList()

    init {
        editor.document.addDocumentListener(object : DocumentListener {
            override fun documentChanged(event: DocumentEvent) = clear(editor)
        }, this)
    }

    override fun dispose() {
        val live = regions.filter { it.isValid }
        if (live.isNotEmpty() && !editor.isDisposed) {
            editor.foldingModel.runBatchFoldingOperation { live.forEach(editor.foldingModel::removeFoldRegion) }
        }
    }
}

private fun clear(editor: Editor) {
    editor.getUserData(PSEUDO)?.let { editor.putUserData(PSEUDO, null); Disposer.dispose(it) }
}

/** Adds the regions; a body whose exact range is taken (the IDE's own method fold) folds just inside its braces. */
private fun apply(editor: Editor, regions: List<PseudoRegion>) {
    clear(editor)
    val pseudo = Pseudo(editor)
    val added = mutableListOf<FoldRegion>()
    val model = editor.foldingModel
    model.runBatchFoldingOperation {
        for (r in regions) {
            val region = model.addFoldRegion(r.start, r.end, r.placeholder)
                ?: model.getFoldRegion(r.start, r.end)?.takeIf { it.placeholderText != r.placeholder }?.let {
                    model.addFoldRegion(r.start + 1, r.end - 1, r.placeholder.removePrefix("{").removeSuffix("}"))
                }
                ?: continue
            region.isExpanded = r.expanded
            added += region
        }
    }
    pseudo.regions = added
    editor.putUserData(PSEUDO, pseudo)
    // Goes with the editor; outside an EditorImpl (tests, previews) with the toggle or the next edit.
    (editor as? EditorImpl)?.let { Disposer.register(it.disposable, pseudo) }
}

private fun notify(project: Project?, message: String) =
    NotificationGroupManager.getInstance().getNotificationGroup("Wish").createNotification(message, NotificationType.WARNING).notify(project)

/** Runs `wishlang pseudo --json -` on [text]; null and a notification when it fails. */
private fun pseudoOf(project: Project?, text: String): PseudoFile? {
    val bin = wishBinary()
    if (!bin.canExecute()) {
        project?.let(::notifyMissing)
        return null
    }
    val p = ProcessBuilder(bin.path, "pseudo", "--json", "-").apply { environment().putAll(EnvironmentUtil.getEnvironmentMap()) }.start()
    p.outputStream.use { it.write(text.toByteArray()) }
    val out = p.inputStream.bufferedReader().readText()
    val err = p.errorStream.bufferedReader().readText()
    if (p.waitFor() != 0) {
        notify(project, "Pseudocode failed: ${err.trim().removePrefix("wishlang: ")}")
        return null
    }
    return Gson().fromJson(out, PseudoFile::class.java)
}

/** Editor and diff-viewer toggle: fold the open Java code into its pseudocode, or back. */
class ReadAsPseudocode : ToggleAction(), DumbAware {
    override fun getActionUpdateThread() = ActionUpdateThread.BGT

    override fun update(e: AnActionEvent) {
        super.update(e)
        e.presentation.isEnabledAndVisible = e.getData(CommonDataKeys.EDITOR) != null
    }

    override fun isSelected(e: AnActionEvent) = e.getData(CommonDataKeys.EDITOR)?.getUserData(PSEUDO) != null

    override fun setSelected(e: AnActionEvent, state: Boolean) {
        val editor = e.getData(CommonDataKeys.EDITOR) ?: return
        if (!state) return clear(editor)
        val text = editor.document.text
        val stamp = editor.document.modificationStamp
        object : Task.Backgroundable(e.project, "Reading as pseudocode", true) {
            override fun run(indicator: ProgressIndicator) {
                val file = pseudoOf(project, text) ?: return
                ApplicationManager.getApplication().invokeLater {
                    if (!editor.isDisposed && editor.document.modificationStamp == stamp) apply(editor, pseudoRegions(file))
                }
            }
        }.queue()
    }
}
