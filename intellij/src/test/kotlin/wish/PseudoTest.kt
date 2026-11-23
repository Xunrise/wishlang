package wish

import com.google.gson.Gson
import com.intellij.ide.highlighter.JavaHighlightingColors
import com.intellij.testFramework.PlatformTestUtil
import com.intellij.testFramework.fixtures.BasePlatformTestCase

class PseudoTest : BasePlatformTestCase() {
    private val json = """
        {"folds": [
          {"start": 27, "end": 31, "placeholder": "one", "zoom": 2, "tokens": []},
          {"start": 0, "end": 80, "placeholder": "class A: does a", "zoom": 0, "tokens": [[0, 5, "keyword"], [6, 7, "type"], [3, 99, "type"]]},
          {"start": 20, "end": 50, "placeholder": "f(): does f", "zoom": 1},
          {"start": 0, "end": 9, "placeholder": "class A", "zoom": 255},
          {"start": 40, "end": 40, "placeholder": "empty", "zoom": 2}
        ]}
    """

    fun testFoldsAreAddedOuterFirstWithValidSpans() {
        val regions = pseudoRegions(Gson().fromJson(json, PseudoFile::class.java))
        assertEquals(listOf("class A: does a", "class A", "f(): does f", "one"), regions.map { it.placeholder })
        assertEquals(listOf(Span(0, 5, "keyword"), Span(6, 7, "type")), regions[0].spans)
    }

    fun testFragmentsUseTheJavaPalette() {
        val f = fragments("class A: does a", listOf(Span(0, 5, "keyword"), Span(6, 7, "type")))
        assertEquals(listOf("class", " ", "A", ": does a"), f.map { it.first })
        assertEquals(listOf(JavaHighlightingColors.KEYWORD, null, JavaHighlightingColors.CLASS_NAME_ATTRIBUTES, null), f.map { it.second })
    }

    fun testNestedFoldsOpenOneLevelAtATimeWithOneInlayPerVisibleFold() {
        myFixture.configureByText("A.java", "class A {\n  void f() {\n    a();\n    b();\n  }\n}\n")
        val editor = myFixture.editor
        val text = editor.document.text
        val cls = PseudoRegion(0, text.lastIndexOf('}') + 1, "class A: does a")
        val head = PseudoRegion(0, text.indexOf('{') + 1, "class A")
        val f = PseudoRegion(text.indexOf("void"), text.indexOf("  }") + 3, "f(): does f")
        val sig = PseudoRegion(text.indexOf("void"), text.indexOf("() {") + 2, "f()")
        val step = PseudoRegion(text.indexOf("a();"), text.indexOf("b();") + 4, "a then b")
        applyPseudo(editor, listOf(cls, head, f, sig, step))
        val ours = editor.foldingModel.allFoldRegions.filter { it.placeholderText.isEmpty() }
        assertEquals(5, ours.size)
        assertTrue(ours.none { it.isExpanded })
        // Only the class fold is on screen: one inlay, drawn where the class starts.
        val inlays = editor.inlayModel.getInlineElementsInRange(0, text.length)
        assertEquals(listOf(cls.end), inlays.map { it.offset })
        assertEquals(editor.offsetToXY(0).x, inlays.single().bounds!!.x)

        // Open the class: its heading and the method show, each as its own inlay.
        val clsRegion = ours.first { it.endOffset == cls.end }
        editor.foldingModel.runBatchFoldingOperation { clsRegion.isExpanded = true }
        PlatformTestUtil.dispatchAllInvocationEventsInIdeEventQueue()
        val after = editor.inlayModel.getInlineElementsInRange(0, text.length)
        assertEquals(listOf(head.end, f.end), after.map { it.offset }.sorted())
        assertTrue("every inlay is drawn", after.all { it.bounds != null })
    }
}
