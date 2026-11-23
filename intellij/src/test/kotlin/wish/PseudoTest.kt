package wish

import com.google.gson.Gson
import com.intellij.testFramework.fixtures.BasePlatformTestCase

class PseudoTest : BasePlatformTestCase() {
    private val json = """
        {"folds": [
          {"start": 27, "end": 31, "placeholder": "one", "zoom": 2},
          {"start": 0, "end": 80, "placeholder": "class A: does a", "zoom": 0},
          {"start": 20, "end": 50, "placeholder": "f(): does f", "zoom": 1},
          {"start": 0, "end": 9, "placeholder": "class A", "zoom": 255},
          {"start": 40, "end": 40, "placeholder": "empty", "zoom": 2}
        ]}
    """

    fun testFoldsAreAddedOuterFirstAndEmptyOnesDropped() {
        val regions = pseudoRegions(Gson().fromJson(json, PseudoFile::class.java))
        assertEquals(listOf("class A: does a", "class A", "f(): does f", "one"), regions.map { it.placeholder })
    }

    fun testNestedFoldsOpenOneLevelAtATime() {
        myFixture.configureByText("A.java", "class A {\n  void f() {\n    a();\n    b();\n  }\n}\n")
        val editor = myFixture.editor
        val text = editor.document.text
        val cls = PseudoRegion(0, text.lastIndexOf('}') + 1, "class A: does a")
        val head = PseudoRegion(0, text.indexOf('{') + 1, "class A")
        val f = PseudoRegion(text.indexOf("void"), text.indexOf("  }") + 3, "f(): does f")
        val sig = PseudoRegion(text.indexOf("void"), text.indexOf("() {") + 2, "f()")
        val step = PseudoRegion(text.indexOf("a();"), text.indexOf("b();") + 4, "a then b")
        com.intellij.openapi.application.ApplicationManager.getApplication().invokeAndWait {
            applyPseudo(editor, listOf(cls, head, f, sig, step))
        }
        val added = editor.foldingModel.allFoldRegions.map { it.placeholderText }
        assertTrue(added.toString(), added.containsAll(listOf("class A: does a", "class A", "f(): does f", "f()", "a then b")))
        assertTrue(editor.foldingModel.allFoldRegions.filter { it.placeholderText in setOf("class A: does a", "a then b") }.none { it.isExpanded })
    }
}
