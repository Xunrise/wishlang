package wish

import com.google.gson.Gson
import com.intellij.testFramework.fixtures.BasePlatformTestCase

class PseudoTest : BasePlatformTestCase() {
    private val json = """
        {"methods": [
          {"path": "A.f", "outline": "does f", "body": [21, 50], "fallback": null,
           "folds": [{"start": 27, "end": 31, "placeholder": "one"}, {"start": 36, "end": 46, "placeholder": "  two"}]},
          {"path": "A.g", "outline": "does g", "body": [62, 64], "folds": []}
        ]}
    """

    fun testMethodsFoldToOutlineAroundTheirSteps() {
        val regions = pseudoRegions(Gson().fromJson(json, PseudoFile::class.java))
        assertEquals(
            listOf(
                PseudoRegion(21, 50, "{ does f }", expanded = true),
                PseudoRegion(27, 31, "one", expanded = false),
                PseudoRegion(36, 46, "  two", expanded = false),
                PseudoRegion(62, 64, "{ does g }", expanded = false),
            ),
            regions,
        )
    }
}
