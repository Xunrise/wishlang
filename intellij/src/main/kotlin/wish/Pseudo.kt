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
import com.intellij.ide.highlighter.JavaHighlightingColors
import com.intellij.ide.ui.UISettings
import com.intellij.openapi.editor.EditorCustomElementRenderer
import com.intellij.openapi.editor.HighlighterColors
import com.intellij.openapi.editor.Inlay
import com.intellij.openapi.editor.colors.EditorFontType
import com.intellij.openapi.editor.colors.TextAttributesKey
import com.intellij.openapi.editor.event.EditorMouseEvent
import com.intellij.openapi.editor.event.EditorMouseListener
import com.intellij.openapi.editor.ex.FoldingListener
import com.intellij.openapi.editor.ex.FoldingModelEx
import com.intellij.openapi.editor.markup.TextAttributes
import java.awt.Color
import java.awt.Font
import java.awt.Graphics
import java.awt.Rectangle
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
 * methods, then their steps) plus detail folds (imports, headings, fields, signatures). `tokens` are
 * `[start, end, kind]` spans of the placeholder, in its UTF-16 units.
 */
data class PseudoFile(val folds: List<PseudoFold> = emptyList())
data class PseudoFold(val start: Int = 0, val end: Int = 0, val placeholder: String = "", val zoom: Int = 0, val tokens: List<List<Any>> = emptyList())

data class Span(val start: Int, val end: Int, val kind: String)

/** A fold to add. All start collapsed: the file opens as one line per class, and each fold opens one level. */
data class PseudoRegion(val start: Int, val end: Int, val placeholder: String, val spans: List<Span> = emptyList())

/** Outer folds first, so nested ones are added inside them. */
fun pseudoRegions(file: PseudoFile): List<PseudoRegion> =
    file.folds.filter { it.end > it.start }.sortedWith(compareBy({ it.start }, { -it.end })).map { f ->
        val spans = f.tokens.mapNotNull { t ->
            val s = (t.getOrNull(0) as? Number)?.toInt()
            val e = (t.getOrNull(1) as? Number)?.toInt()
            val k = t.getOrNull(2) as? String
            if (s != null && e != null && k != null && 0 <= s && s < e && e <= f.placeholder.length) Span(s, e, k) else null
        }
        PseudoRegion(f.start, f.end, f.placeholder, spans)
    }

/** The editor's Java colours for each kind of pseudocode word. */
private val COLORS: Map<String, TextAttributesKey> = mapOf(
    "keyword" to JavaHighlightingColors.KEYWORD,
    "method_decl" to JavaHighlightingColors.METHOD_DECLARATION_ATTRIBUTES,
    "method_call" to JavaHighlightingColors.METHOD_CALL_ATTRIBUTES,
    "parameter" to JavaHighlightingColors.PARAMETER_ATTRIBUTES,
    "field" to JavaHighlightingColors.INSTANCE_FINAL_FIELD_ATTRIBUTES,
    "constant" to JavaHighlightingColors.STATIC_FINAL_FIELD_ATTRIBUTES,
    "type" to JavaHighlightingColors.CLASS_NAME_ATTRIBUTES,
    "string" to JavaHighlightingColors.STRING,
    "number" to JavaHighlightingColors.NUMBER,
    "annotation" to JavaHighlightingColors.ANNOTATION_NAME_ATTRIBUTES,
    "operator" to JavaHighlightingColors.OPERATION_SIGN,
)

/** The placeholder cut into runs of one colour: `(text, key)`, key null for plain text. */
fun fragments(text: String, spans: List<Span>): List<Pair<String, TextAttributesKey?>> {
    val out = mutableListOf<Pair<String, TextAttributesKey?>>()
    var at = 0
    for (s in spans.sortedBy { it.start }) {
        if (s.start < at) continue
        if (s.start > at) out += text.substring(at, s.start) to null
        out += text.substring(s.start, s.end) to COLORS[s.kind]
        at = s.end
    }
    if (at < text.length) out += text.substring(at) to null
    return out
}

/**
 * Draws a collapsed fold's pseudocode like code: each word in the scheme's colour and font style for
 * what it is. The fold itself has an empty placeholder; this inlay sits at its end offset, because the
 * editor hides inlays at a collapsed fold's start, and an empty fold leaves its end at the same spot.
 */
private class PseudoRenderer(val region: FoldRegion, val text: String, val spans: List<Span>) : EditorCustomElementRenderer {
    private fun styled(editor: Editor): List<Triple<String, Font, Color>> {
        val scheme = editor.colorsScheme
        return fragments(text, spans).map { (t, key) ->
            val attrs = key?.let(scheme::getAttributes) ?: scheme.getAttributes(HighlighterColors.TEXT)
            val style = attrs?.fontType ?: Font.PLAIN
            Triple(t, scheme.getFont(EditorFontType.forJavaStyle(style)), attrs?.foregroundColor ?: scheme.defaultForeground)
        }
    }

    override fun calcWidthInPixels(inlay: Inlay<*>): Int {
        val editor = inlay.editor
        return maxOf(1, styled(editor).sumOf { (t, font, _) -> editor.contentComponent.getFontMetrics(font).stringWidth(t) })
    }

    override fun paint(inlay: Inlay<*>, g: Graphics, target: Rectangle, textAttributes: TextAttributes) {
        val editor = inlay.editor
        UISettings.setupAntialiasing(g)
        var x = target.x
        val y = target.y + editor.ascent
        for ((t, font, color) in styled(editor)) {
            g.font = font
            g.color = color
            g.drawString(t, x, y)
            x += g.getFontMetrics(font).stringWidth(t)
        }
    }
}

private val PSEUDO = Key.create<Pseudo>("wish.pseudo")

/** The pseudo folds of one editor, gone on toggle-off or on the first edit (their offsets would be stale). */
private class Pseudo(val editor: Editor) : Disposable {
    /** Our fold regions and what each one reads as. */
    var regions: Map<FoldRegion, PseudoRegion> = emptyMap()
    /** The IDE's own folds that crossed ours (a code block's braces inside a step): rebuilt when we go. */
    var displaced = false
    private var inlays: List<Inlay<*>> = emptyList()

    init {
        editor.document.addDocumentListener(object : DocumentListener {
            override fun documentChanged(event: DocumentEvent) = clear(editor)
        }, this)
        (editor.foldingModel as? FoldingModelEx)?.addListener(object : FoldingListener {
            override fun onFoldProcessingEnd() = ApplicationManager.getApplication().invokeLater { if (!editor.isDisposed && editor.getUserData(PSEUDO) === this@Pseudo) refresh() }
        }, this)
        editor.addEditorMouseListener(object : EditorMouseListener {
            override fun mouseClicked(e: EditorMouseEvent) {
                val r = (e.inlay?.renderer as? PseudoRenderer)?.region ?: return
                if (r.isValid) editor.foldingModel.runBatchFoldingOperation { r.isExpanded = true }
                e.consume()
            }
        }, this)
    }

    /** One inlay per collapsed fold that is not inside another collapsed fold: exactly the ones on screen. */
    fun refresh() {
        inlays.forEach(Disposer::dispose)
        val collapsed = editor.foldingModel.allFoldRegions.filter { it.isValid && !it.isExpanded }
        inlays = regions.filter { (r, _) ->
            r.isValid && !r.isExpanded && collapsed.none { o ->
                o !== r && o.startOffset <= r.startOffset && r.endOffset <= o.endOffset &&
                    // Same start: the longer one is the outer fold (a class around its heading).
                    (o.startOffset < r.startOffset || o.endOffset > r.endOffset)
            }
        }.mapNotNull { (r, p) -> editor.inlayModel.addInlineElement(r.endOffset, false, PseudoRenderer(r, p.placeholder, p.spans)) }
    }

    override fun dispose() {
        inlays.forEach(Disposer::dispose)
        if (editor.isDisposed) return
        val live = regions.keys.filter { it.isValid }
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
    val added = linkedMapOf<FoldRegion, PseudoRegion>()
    val model = editor.foldingModel
    model.runBatchFoldingOperation {
        for (r in regions) {
            model.allFoldRegions.filter { it !in added && clashes(it, r.start, r.end) }.forEach {
                model.removeFoldRegion(it)
                pseudo.displaced = true
            }
            // Empty placeholder: the inlay draws the text, in colour.
            val region = model.addFoldRegion(r.start, r.end, "") ?: continue
            region.isExpanded = false
            added[region] = r
        }
    }
    pseudo.regions = added
    editor.putUserData(PSEUDO, pseudo)
    pseudo.refresh()
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
