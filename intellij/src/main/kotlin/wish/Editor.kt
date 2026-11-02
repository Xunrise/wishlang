package wish

import com.intellij.codeInsight.daemon.LineMarkerInfo
import com.intellij.codeInsight.daemon.LineMarkerProvider
import com.intellij.codeInsight.daemon.impl.HighlightInfo
import com.intellij.codeInsight.daemon.impl.HighlightInfoFilter
import com.intellij.codeInsight.hint.HintManager
import com.intellij.icons.AllIcons
import com.intellij.lang.annotation.AnnotationHolder
import com.intellij.lang.annotation.Annotator
import com.intellij.lang.annotation.HighlightSeverity
import com.intellij.openapi.components.service
import com.intellij.openapi.editor.Document
import com.intellij.openapi.editor.Editor
import com.intellij.openapi.editor.EditorFactory
import com.intellij.openapi.editor.colors.TextAttributesKey
import com.intellij.openapi.editor.markup.ActiveGutterRenderer
import com.intellij.openapi.editor.markup.GutterIconRenderer
import com.intellij.openapi.editor.markup.HighlighterLayer
import com.intellij.openapi.editor.markup.HighlighterTargetArea
import com.intellij.openapi.editor.markup.LineMarkerRendererEx
import com.intellij.openapi.editor.markup.RangeHighlighter
import com.intellij.openapi.editor.markup.TextAttributes
import com.intellij.openapi.fileEditor.FileDocumentManager
import com.intellij.openapi.project.DumbAware
import com.intellij.openapi.project.Project
import com.intellij.openapi.ui.popup.JBPopupFactory
import com.intellij.openapi.util.Key
import com.intellij.openapi.util.TextRange
import com.intellij.openapi.util.text.StringUtil
import com.intellij.openapi.vfs.VirtualFile
import com.intellij.psi.PsiElement
import com.intellij.psi.PsiFile
import com.intellij.psi.PsiWhiteSpace
import com.intellij.ui.JBColor
import com.intellij.ui.SimpleListCellRenderer
import com.intellij.ui.awt.RelativePoint
import org.eclipse.lsp4j.Position
import org.eclipse.lsp4j.Range
import java.awt.Font
import java.awt.Graphics
import java.awt.Rectangle
import java.awt.event.MouseEvent

object WishColors {
    /** The gutter bar is the primary signal, so the tints are barely there. */
    val GENERATED = TextAttributesKey.createTextAttributesKey(
        "WISH_GENERATED", TextAttributes(null, JBColor(0xF6F9FF, 0x21252C), null, null, Font.PLAIN),
    )
    val PROTOTYPE = TextAttributesKey.createTextAttributesKey(
        "WISH_PROTOTYPE", TextAttributes(null, JBColor(0xEDF3FE, 0x242C3B), null, null, Font.PLAIN),
    )
    val BAR = JBColor(0x3B82F6, 0x4F8EF7)
    val UNTRACED = JBColor(0xDC2626, 0xF87171)
}

/** Just enough markdown for wish's hover text: bold, code and paragraphs. */
fun mdToHtml(md: String): String = "<html>" + mdInline(md) + "</html>"

internal fun mdInline(md: String?): String = StringUtil.escapeXmlEntities(md.orEmpty())
    .replace(Regex("\\*\\*(.+?)\\*\\*"), "<b>$1</b>")
    .replace(Regex("`([^`]+)`"), "<code>$1</code>")
    .replace("\n\n", "<br><br>")
    .replace("\n", "<br>")

internal fun esc(s: String?): String = StringUtil.escapeXmlEntities(s.orEmpty())

fun Document.offset(p: Position): Int =
    if (p.line >= lineCount) textLength else (getLineStartOffset(p.line) + p.character).coerceAtMost(getLineEndOffset(p.line))

fun Document.range(r: Range) = TextRange(offset(r.start), offset(r.end))

private fun Document.blockRange(b: Block) = TextRange(getLineStartOffset(b.start), getLineEndOffset(b.end.coerceAtMost(lineCount - 1)))

fun stateOf(file: PsiFile): FileState? =
    file.virtualFile?.let { file.project.service<WishService>().states[uriOf(it)] }

// ---- gutter bars ------------------------------------------------------------------------------

private val BARS = Key.create<List<RangeHighlighter>>("wish.bars")

/** A 3px bar beside each block of generated code; hovering or clicking it shows wish's reasoning. */
private class BlockBar(private val html: String, private val untraced: Boolean) : ActiveGutterRenderer, LineMarkerRendererEx {
    override fun paint(editor: Editor, g: Graphics, r: Rectangle) {
        g.color = if (untraced) WishColors.UNTRACED else WishColors.BAR
        g.fillRect(r.x, r.y, 3, r.height)
    }

    override fun getPosition() = LineMarkerRendererEx.Position.RIGHT
    override fun getTooltipText() = html
    override fun canDoAction(e: MouseEvent) = true
    override fun doAction(editor: Editor, e: MouseEvent) = HintManager.getInstance().showInformationHint(editor, html)
}

fun paintBars(project: Project, file: VirtualFile, state: FileState) {
    val document = FileDocumentManager.getInstance().getCachedDocument(file) ?: return
    for (editor in EditorFactory.getInstance().getEditors(document, project)) {
        editor.getUserData(BARS)?.forEach { editor.markupModel.removeHighlighter(it) }
        val bars = state.blocks.filter { it.start < document.lineCount }.map { b ->
            val range = document.blockRange(b)
            editor.markupModel.addRangeHighlighter(range.startOffset, range.endOffset, HighlighterLayer.FIRST, null, HighlighterTargetArea.LINES_IN_RANGE)
                .apply { lineMarkerRenderer = BlockBar(mdToHtml(b.hover), b.untraced) }
        }
        editor.putUserData(BARS, bars)
    }
}

// ---- highlighting -----------------------------------------------------------------------------

/** Paints the file's state; it is already in memory, so one pass over the file element is all it takes. */
class WishAnnotator : Annotator, DumbAware {
    override fun annotate(element: PsiElement, holder: AnnotationHolder) {
        if (element !is PsiFile) return
        val state = stateOf(element) ?: return
        val document = element.viewProvider.document ?: return
        fun tint(r: Range, key: TextAttributesKey) {
            val range = document.range(r)
            if (!range.isEmpty) holder.newSilentAnnotation(HighlightSeverity.INFORMATION).range(range).textAttributes(key).create()
        }
        state.generated.forEach { tint(it, WishColors.GENERATED) }
        state.prototype.forEach { tint(it, WishColors.PROTOTYPE) }
        state.blocks.filter { it.start < document.lineCount }.forEach {
            holder.newSilentAnnotation(HighlightSeverity.INFORMATION).range(document.blockRange(it)).tooltip(mdToHtml(it.hover)).create()
        }
        state.pending.forEach {
            val range = document.range(it.range)
            if (!range.isEmpty) holder.newAnnotation(HighlightSeverity.WEAK_WARNING, "✦ will be generated: ${it.name}").range(range).create()
        }
    }
}

/** Hides IntelliJ's "cannot resolve" errors on symbols wish is about to generate. */
class WishInfoFilter : HighlightInfoFilter {
    override fun accept(info: HighlightInfo, file: PsiFile?): Boolean {
        if (file == null || info.severity < HighlightSeverity.ERROR) return true
        val pending = stateOf(file)?.pending.orEmpty().ifEmpty { return true }
        val document = file.viewProvider.document ?: return true
        return pending.none { document.range(it.range).intersects(info.startOffset, info.endOffset) }
    }
}

// ---- lenses -----------------------------------------------------------------------------------

/** A lens runs its command on the server, except `wish.ticket.open`, which selects the problem in the Ticket tab. */
internal fun runLens(project: Project, lens: Lens) {
    if (lens.command == "wish.ticket.open") showProblem(project, lens.arguments.firstOrNull()?.asString.orEmpty())
    else project.service<WishService>().execute(lens.command, lens.arguments)
}

class WishLenses : LineMarkerProvider {
    override fun getLineMarkerInfo(element: PsiElement): LineMarkerInfo<*>? {
        if (element.firstChild != null || element is PsiWhiteSpace) return null
        val file = element.containingFile ?: return null
        val document = file.viewProvider.document ?: return null
        val offset = element.textRange.startOffset
        if (offset >= document.textLength) return null
        val line = document.getLineNumber(offset)
        val here = stateOf(file)?.lenses.orEmpty().filter { it.line == line }.ifEmpty { return null }
        // Only the first non-blank leaf of the line carries the marker.
        val first = (document.getLineStartOffset(line) until document.getLineEndOffset(line)).first { !document.charsSequence[it].isWhitespace() }
        if (first != offset) return null
        return LineMarkerInfo(
            element, element.textRange, AllIcons.Actions.IntentionBulb,
            { here.joinToString(" | ") { it.title } },
            { event, leaf ->
                if (here.size == 1) runLens(leaf.project, here[0])
                else JBPopupFactory.getInstance().createPopupChooserBuilder(here)
                    .setRenderer(SimpleListCellRenderer.create("") { it.title })
                    .setItemChosenCallback { runLens(leaf.project, it) }
                    .createPopup().show(RelativePoint(event))
            },
            GutterIconRenderer.Alignment.RIGHT,
            { "Wish: " + here.first().title },
        )
    }
}
