package com.example.library

import java.time.LocalDate

data class Book(val isbn: String, val title: String, val author: String, val year: Int)

data class Loan(val book: Book, val member: String, val due: LocalDate)

sealed interface LoanResult {
    data class Ok(val loan: Loan) : LoanResult
    data class Unavailable(val reason: String) : LoanResult
}

class Library(private val loanDays: Long = 14) {
    private val books = linkedMapOf<String, Book>()
    private val loans = mutableMapOf<String, Loan>()

    fun add(book: Book) {
        require(book.isbn.isNotBlank()) { "ISBN required" }
        books[book.isbn] = book
    }

    fun search(query: String): List<Book> =
        books.values.filter { query.lowercase() in it.title.lowercase() || query.lowercase() in it.author.lowercase() }

    fun lend(isbn: String, member: String, today: LocalDate = LocalDate.now()): LoanResult {
        val book = books[isbn] ?: return LoanResult.Unavailable("unknown ISBN $isbn")
        if (isbn in loans) return LoanResult.Unavailable("'${book.title}' is already on loan")
        val loan = Loan(book, member, today.plusDays(loanDays))
        loans[isbn] = loan
        return LoanResult.Ok(loan)
    }

    fun giveBack(isbn: String): Boolean = loans.remove(isbn) != null

    // TODO: fines for overdue loans
    fun overdue(today: LocalDate): List<Loan> = loans.values.filter { it.due.isBefore(today) }

    val size: Int get() = books.size
}
