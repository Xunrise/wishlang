package wish

import com.google.gson.Gson
import com.intellij.codeInsight.folding.CodeFoldingManager
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

/**
 * `wishlang pseudo --json`: every fold in Document offsets, nested as zoom levels (class, then its
 * methods, then their steps) plus detail folds (imports, headings, fields, signatures).
 */
data class PseudoFile(val folds: List<PseudoFold> = emptyList())
data class PseudoFold(val start: Int = 0, val end: Int = 0, val placeholder: String = "", val zoom: Int = 0)

/** A fold to add. All start collapsed: the file opens as one line per class, and each fold opens one level. */
data class PseudoRegion(val start: Int, val end: Int, val placeholder: String)

/** Outer folds first, so nested ones are added inside them. */
fun pseudoRegions(file: PseudoFile): List<PseudoRegion> =
    file.folds.filter { it.end > it.start }.sortedWith(compareBy({ it.start }, { -it.end })).map { PseudoRegion(it.start, it.end, it.placeholder) }

private val PSEUDO = Key.create<Pseudo>("wish.pseudo")

/** The pseudo folds of one editor, gone on toggle-off or on the first edit (their offsets would be stale). */
private class Pseudo(val editor: Editor) : Disposable {
    var regions: List<FoldRegion> = emptyList()
    /** The IDE's own folds that crossed ours (a code block's braces inside a step): rebuilt when we go. */
    var displaced = false

    init {
        editor.document.addDocumentListener(object : DocumentListener {
            override fun documentChanged(event: DocumentEvent) = clear(editor)
        }, this)
    }

    override fun dispose() {
        if (editor.isDisposed) return
        val live = regions.filter { it.isValid }
        if (live.isNotEmpty()) editor.foldingModel.runBatchFoldingOperation { live.forEach(editor.foldingModel::removeFoldRegion) }
        if (displaced) editor.project?.let { CodeFoldingManager.getInstance(it).updateFoldRegions(editor) }
    }
}

private fun clear(editor: Editor) {
    editor.getUserData(PSEUDO)?.let { editor.putUserData(PSEUDO, null); Disposer.dispose(it) }
}

/** Does [o] cross the range without one containing the other, or cover exactly the same range? */
private fun clashes(o: FoldRegion, start: Int, end: Int): Boolean {
    val same = o.startOffset == start && o.endOffset == end
    val crosses = o.startOffset < end && start < o.endOffset &&
        !(o.startOffset <= start && end <= o.endOffset) && !(start <= o.startOffset && o.endOffset <= end)
    return same || crosses
}

/** Adds the regions, collapsed. The IDE's own folds that clash with one (it folds every `{}` block) make way. */
internal fun applyPseudo(editor: Editor, regions: List<PseudoRegion>) {
    clear(editor)
    val pseudo = Pseudo(editor)
    val added = mutableListOf<FoldRegion>()
    val model = editor.foldingModel
    model.runBatchFoldingOperation {
        for (r in regions) {
            model.allFoldRegions.filter { it !in added && clashes(it, r.start, r.end) }.forEach {
                model.removeFoldRegion(it)
                pseudo.displaced = true
            }
            val region = model.addFoldRegion(r.start, r.end, r.placeholder) ?: continue
            region.isExpanded = false
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
                    if (!editor.isDisposed && editor.document.modificationStamp == stamp) applyPseudo(editor, pseudoRegions(file))
                }
            }
        }.queue()
    }
}
