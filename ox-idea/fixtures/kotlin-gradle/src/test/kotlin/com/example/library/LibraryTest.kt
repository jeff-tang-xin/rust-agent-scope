package com.example.library

import java.time.LocalDate
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertTrue
import org.junit.jupiter.api.Assumptions.assumeTrue

class LibraryTest {
    private val day = LocalDate.of(2024, 3, 1)

    private fun library() = Library(loanDays = 7).apply {
        add(Book("1", "Kotlin in Action", "Jemerov", 2017))
        add(Book("2", "Java Concurrency in Practice", "Goetz", 2006))
    }

    @Test
    fun `search matches title and author`() {
        assertEquals(listOf("2"), library().search("goetz").map { it.isbn })
        assertEquals(2, library().search("in").size)
    }

    @Test
    fun `cannot lend a book twice`() {
        val lib = library()
        assertIs<LoanResult.Ok>(lib.lend("1", "alice", day))
        assertIs<LoanResult.Unavailable>(lib.lend("1", "bob", day))
        assertTrue(lib.giveBack("1"))
    }

    @Test
    fun `overdue loans`() {
        val lib = library()
        lib.lend("2", "carol", day)
        assertEquals(1, lib.overdue(day.plusDays(8)).size)
    }

    @Test
    fun `intentionally failing`() {
        assumeTrue(System.getProperty("openidea.fail") == "true", "run with -PopenideaFail")
        // FIXME: wrong on purpose (QA: failing test in the test tree)
        assertEquals(3, library().size)
    }
}
