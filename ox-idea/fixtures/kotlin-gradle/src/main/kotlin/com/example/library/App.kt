package com.example.library

import java.time.LocalDate

fun main() {
    val library = Library()
    library.add(Book("978-0134685991", "Effective Java", "Joshua Bloch", 2018))
    library.add(Book("978-1617293290", "Kotlin in Action", "Dmitry Jemerov", 2017))
    library.add(Book("978-0201633610", "Design Patterns", "Erich Gamma", 1994))

    val unused = "intentional warning: unused variable"
    // Set a breakpoint here (QA: debugger) and inspect `library`.
    when (val result = library.lend("978-1617293290", "alice", LocalDate.of(2024, 1, 1))) {
        is LoanResult.Ok -> println("Lent '${result.loan.book.title}' until ${result.loan.due}")
        is LoanResult.Unavailable -> println("Cannot lend: ${result.reason}")
    }
    library.search("java").forEach { println("Found: ${it.title} (${it.year})") }
}
