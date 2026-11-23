import Foundation

@MainActor
final class Session: ObservableObject {
    static let shared = Session()

    @Published var state = "остановлен"
    @Published var phrase = "фразы ещё не было"
    @Published var command = ""
    @Published var commandOK = true
    @Published var note = ""
    @Published var name = "Помощник"
    @Published var commands: [CommandRow] = []
    @Published var bookmarks: [BookmarkRow] = []

    private var process: Process?
    private var output = ""
    private let database = AssistantDatabase()
    private var root: URL?

    func prepare() {
        let root = projectRoot()
        self.root = root
        name = assistantName(in: root) ?? "Помощник"
        reloadLists()
        if process == nil {
            start()
        }
    }

    func start() {
        guard process == nil else { return }
        guard let root else {
            note = "не нашёл каталог проекта"
            return
        }
        if otherAssistantIsRunning() {
            note = "помощник уже слушает в другом окне. Остановите его и нажмите «Запустить», иначе микрофон будет занят дважды"
            state = "остановлен"
            return
        }
        let binary = assistantBinary(in: root)
        guard FileManager.default.isExecutableFile(atPath: binary.path) else {
            note = "нет собранного помощника. В каталоге проекта выполните cargo build --release"
            state = "остановлен"
            return
        }
        let pipe = Pipe()
        let child = Process()
        child.executableURL = binary
        child.currentDirectoryURL = root
        child.arguments = ["--language", "ru"]
        child.standardOutput = pipe
        child.standardError = pipe
        child.terminationHandler = { _ in
            Task { @MainActor in
                if Session.shared.process === child {
                    Session.shared.process = nil
                    Session.shared.state = "остановлен"
                }
            }
        }
        do {
            try child.run()
        } catch {
            note = "не запустил помощника: \(error.localizedDescription)"
            return
        }
        process = child
        state = "загрузка"
        note = ""
        pipe.fileHandleForReading.readabilityHandler = { handle in
            let data = handle.availableData
            guard !data.isEmpty, let text = String(data: data, encoding: .utf8) else { return }
            Task { @MainActor in
                Session.shared.takeOutput(text)
            }
        }
    }

    func stop() {
        process?.terminationHandler = nil
        process?.terminate()
        process = nil
        state = "остановлен"
    }

    func reloadLists() {
        guard let root else { return }
        let path = root.appendingPathComponent("data/assistant.db").path
        guard FileManager.default.fileExists(atPath: path) else {
            note = "базы ещё нет"
            return
        }
        database.close()
        do {
            try database.open(path: path)
            commands = try database.loadCommands()
            bookmarks = try database.loadBookmarks()
        } catch {
            note = String(describing: error)
        }
    }

    func addCommand() {
        commands.append(CommandRow(rowID: 0, phrase: "", action: "open_app", target: ""))
    }

    func addBookmark() {
        bookmarks.append(BookmarkRow(rowID: 0, name: "", url: "https://"))
    }

    func save() {
        let phrases = commands.map { $0.phrase.trimmingCharacters(in: .whitespaces) }
        if phrases.contains(where: \.isEmpty) || bookmarks.contains(where: { $0.name.trimmingCharacters(in: .whitespaces).isEmpty || $0.url.trimmingCharacters(in: .whitespaces).isEmpty }) {
            note = "заполните фразу, имя закладки и адрес"
            return
        }
        if Set(phrases.map { $0.lowercased() }).count != phrases.count {
            note = "фразы команд не должны повторяться"
            return
        }
        do {
            try database.save(commands: commands, bookmarks: bookmarks)
            reloadLists()
            note = "сохранено. Новая команда заработает со следующей фразы «пуск»"
        } catch {
            note = String(describing: error)
        }
    }

    private func takeOutput(_ chunk: String) {
        output.append(chunk)
        while let newline = output.firstIndex(of: "\n") {
            let line = String(output[..<newline])
            output = String(output[output.index(after: newline)...])
            apply(line)
        }
    }

    private func apply(_ line: String) {
        guard line.hasPrefix("событие ") else { return }
        let raw = line.dropFirst("событие ".count)
        guard let data = raw.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let kind = object["тип"] as? String else { return }
        switch kind {
        case "состояние":
            state = object["значение"] as? String ?? state
        case "фраза":
            phrase = object["текст"] as? String ?? phrase
        case "команда":
            command = object["текст"] as? String ?? ""
            commandOK = object["успех"] as? Bool ?? false
        default:
            break
        }
    }
}

private func projectRoot() -> URL {
    if let given = ProcessInfo.processInfo.environment["VOICE_ASSISTANT_ROOT"] {
        return URL(fileURLWithPath: given, isDirectory: true)
    }
    var url = Bundle.main.bundleURL
    for _ in 0..<6 {
        if FileManager.default.fileExists(atPath: url.appendingPathComponent("Cargo.toml").path) {
            return url
        }
        url.deleteLastPathComponent()
    }
    return URL(fileURLWithPath: FileManager.default.currentDirectoryPath, isDirectory: true)
}

private func assistantBinary(in root: URL) -> URL {
    let release = root.appendingPathComponent("target/release/voice-assistant")
    if FileManager.default.isExecutableFile(atPath: release.path) {
        return release
    }
    return root.appendingPathComponent("target/debug/voice-assistant")
}

private func otherAssistantIsRunning() -> Bool {
    let pipe = Pipe()
    let task = Process()
    task.executableURL = URL(fileURLWithPath: "/usr/bin/pgrep")
    task.arguments = ["-x", "voice-assistant"]
    task.standardOutput = pipe
    try? task.run()
    task.waitUntilExit()
    let data = pipe.fileHandleForReading.readDataToEndOfFile()
    let text = String(data: data, encoding: .utf8) ?? ""
    return !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
}

private func assistantName(in root: URL) -> String? {
    guard let text = try? String(contentsOf: root.appendingPathComponent(".env"), encoding: .utf8) else {
        return nil
    }
    for line in text.split(separator: "\n") {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        if trimmed.hasPrefix("ASSISTANT_NAME=") {
            let value = trimmed.dropFirst("ASSISTANT_NAME=".count).trimmingCharacters(in: .whitespaces)
            return value.isEmpty ? nil : String(value)
        }
    }
    return nil
}
