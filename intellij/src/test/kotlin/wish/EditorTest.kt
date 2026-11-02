package wish

import com.google.gson.Gson
import com.google.gson.JsonArray
import com.google.gson.JsonObject
import com.intellij.codeInsight.daemon.impl.HighlightInfo
import com.intellij.lang.annotation.HighlightSeverity
import com.intellij.openapi.components.service
import com.intellij.testFramework.fixtures.BasePlatformTestCase
import java.io.File

class EditorTest : BasePlatformTestCase() {
    private val source = "package demo;\n\npublic class App {\n    static void f() {\n        Missing.call();\n    }\n}\n" + "// padding\n".repeat(10)

    private fun fileState(withPending: Boolean): FileState {
        val json = Gson().fromJson(File("src/test/testData/fileState.json").readText(), JsonObject::class.java)
        if (withPending) json.add("pending", JsonArray().apply {
            add(Gson().toJsonTree(PendingRange(org.eclipse.lsp4j.Range(org.eclipse.lsp4j.Position(4, 8), org.eclipse.lsp4j.Position(4, 15)), "Missing")))
        })
        return Gson().fromJson(json, FileState::class.java)
    }

    private fun highlights(withPending: Boolean): List<HighlightInfo> {
        val file = myFixture.configureByText("App.java", source)
        project.service<WishService>().states[uriOf(file.virtualFile)] = fileState(withPending)
        return myFixture.doHighlighting()
    }

    fun testUnresolvedSymbolIsAnErrorWithoutPendingState() {
        assertTrue(highlights(false).any { it.severity == HighlightSeverity.ERROR && it.description.orEmpty().contains("Cannot resolve symbol") })
    }

    fun testPendingSymbolGetsWeakWarningAndNoResolveError() {
        val infos = highlights(true)
        assertTrue(infos.any { it.severity == HighlightSeverity.WEAK_WARNING && it.description == "✦ will be generated: Missing" })
        assertFalse(infos.any { it.severity == HighlightSeverity.ERROR && it.description.orEmpty().contains("Cannot resolve symbol") })
    }

    fun testGeneratedLinesAreTinted() {
        val tinted = highlights(false).filter { it.forcedTextAttributesKey == WishColors.GENERATED }
        assertEquals(setOf(0, 2, 3, 4), tinted.map { myFixture.editor.document.getLineNumber(it.startOffset) }.filter { it < 5 }.toSet())
    }
}
