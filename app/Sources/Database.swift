import Foundation
import SQLite3

struct CommandRow: Identifiable {
    var token = UUID()
    var rowID: Int64
    var phrase: String
    var action: String
    var target: String
    var id: UUID { token }
}

struct BookmarkRow: Identifiable {
    var token = UUID()
    var rowID: Int64
    var name: String
    var url: String
    var id: UUID { token }
}

enum DatabaseError: Error, CustomStringConvertible {
    case message(String)
    var description: String { if case .message(let text) = self { return text } else { return "" } }
}

final class AssistantDatabase {
    private var db: OpaquePointer?

    func open(path: String) throws {
        if sqlite3_open(path, &db) != SQLITE_OK {
            throw DatabaseError.message(lastError("не открыл базу"))
        }
        try execute("PRAGMA journal_mode=WAL")
        try execute("PRAGMA busy_timeout=3000")
    }

    func close() {
        sqlite3_close(db)
        db = nil
    }

    func loadCommands() throws -> [CommandRow] {
        try query("SELECT id, phrase, action, IFNULL(target, '') FROM commands ORDER BY id") { statement in
            CommandRow(
                rowID: sqlite3_column_int64(statement, 0),
                phrase: column(statement, 1),
                action: column(statement, 2),
                target: column(statement, 3)
            )
        }
    }

    func loadBookmarks() throws -> [BookmarkRow] {
        try query("SELECT id, name, url FROM bookmarks ORDER BY id") { statement in
            BookmarkRow(
                rowID: sqlite3_column_int64(statement, 0),
                name: column(statement, 1),
                url: column(statement, 2)
            )
        }
    }

    func save(commands: [CommandRow], bookmarks: [BookmarkRow]) throws {
        let keptCommands = commands.map(\.rowID).filter { $0 > 0 }
        let keptBookmarks = bookmarks.map(\.rowID).filter { $0 > 0 }
        try execute("BEGIN")
        do {
            try deleteMissing(table: "commands", kept: keptCommands)
            for row in commands {
                if row.rowID > 0 {
                    try run(
                        sql: "UPDATE commands SET phrase = ?, action = ?, target = ?, takes_argument = ? WHERE id = ?",
                        text: [row.phrase, row.action, row.target],
                        ints: [row.action == "open_browser" ? 1 : 0],
                        id: row.rowID
                    )
                } else {
                    try run(
                        sql: "INSERT INTO commands (phrase, action, target, takes_argument) VALUES (?, ?, ?, ?)",
                        text: [row.phrase, row.action, row.target],
                        ints: [row.action == "open_browser" ? 1 : 0],
                        id: nil
                    )
                }
            }
            try deleteMissing(table: "bookmarks", kept: keptBookmarks)
            for row in bookmarks {
                if row.rowID > 0 {
                    try run(
                        sql: "UPDATE bookmarks SET name = ?, url = ? WHERE id = ?",
                        text: [row.name, row.url],
                        ints: [],
                        id: row.rowID
                    )
                } else {
                    try run(
                        sql: "INSERT INTO bookmarks (name, url) VALUES (?, ?)",
                        text: [row.name, row.url],
                        ints: [],
                        id: nil
                    )
                }
            }
            try execute("COMMIT")
        } catch {
            sqlite3_exec(db, "ROLLBACK", nil, nil, nil)
            throw error
        }
    }

    private func deleteMissing(table: String, kept: [Int64]) throws {
        if kept.isEmpty {
            try execute("DELETE FROM \(table)")
            return
        }
        let marks = Array(repeating: "?", count: kept.count).joined(separator: ",")
        var statement: OpaquePointer?
        guard sqlite3_prepare_v2(db, "DELETE FROM \(table) WHERE id NOT IN (\(marks))", -1, &statement, nil) == SQLITE_OK else {
            throw DatabaseError.message(lastError("не подготовил удаление"))
        }
        defer { sqlite3_finalize(statement) }
        for (index, id) in kept.enumerated() {
            sqlite3_bind_int64(statement, Int32(index + 1), id)
        }
        guard sqlite3_step(statement) == SQLITE_DONE else {
            throw DatabaseError.message(lastError("не удалил строку"))
        }
    }

    private func run(sql: String, text: [String], ints: [Int], id: Int64?) throws {
        var statement: OpaquePointer?
        guard sqlite3_prepare_v2(db, sql, -1, &statement, nil) == SQLITE_OK else {
            throw DatabaseError.message(lastError("не подготовил запись"))
        }
        defer { sqlite3_finalize(statement) }
        var index: Int32 = 1
        for value in text {
            sqlite3_bind_text(statement, index, value, -1, unsafeBitCast(-1, to: sqlite3_destructor_type.self))
            index += 1
        }
        for value in ints {
            sqlite3_bind_int(statement, index, Int32(value))
            index += 1
        }
        if let id {
            sqlite3_bind_int64(statement, index, id)
        }
        guard sqlite3_step(statement) == SQLITE_DONE else {
            throw DatabaseError.message(lastError("не записал строку"))
        }
    }

    private func query<T>(_ sql: String, map: (OpaquePointer?) -> T) throws -> [T] {
        var statement: OpaquePointer?
        guard sqlite3_prepare_v2(db, sql, -1, &statement, nil) == SQLITE_OK else {
            throw DatabaseError.message(lastError("не прочитал базу"))
        }
        defer { sqlite3_finalize(statement) }
        var rows: [T] = []
        while sqlite3_step(statement) == SQLITE_ROW {
            rows.append(map(statement))
        }
        return rows
    }

    private func execute(_ sql: String) throws {
        var error: UnsafeMutablePointer<CChar>?
        if sqlite3_exec(db, sql, nil, nil, &error) != SQLITE_OK {
            let text = error.map { String(cString: $0) } ?? "ошибка базы"
            sqlite3_free(error)
            throw DatabaseError.message(text)
        }
    }

    private func column(_ statement: OpaquePointer?, _ index: Int32) -> String {
        guard let value = sqlite3_column_text(statement, index) else { return "" }
        return String(cString: value)
    }

    private func lastError(_ prefix: String) -> String {
        if let raw = sqlite3_errmsg(db) {
            return "\(prefix): \(String(cString: raw))"
        }
        return prefix
    }
}
