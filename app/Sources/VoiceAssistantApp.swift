import AppKit
import SwiftUI

@main
struct VoiceAssistantApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var session = Session.shared

    var body: some Scene {
        MenuBarExtra {
            Panel()
                .environmentObject(session)
        } label: {
            Image(systemName: icon(for: session.state))
        }
        .menuBarExtraStyle(.window)
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        Task { @MainActor in
            Session.shared.prepare()
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        MainActor.assumeIsolated {
            Session.shared.stop()
        }
    }
}

private func icon(for state: String) -> String {
    switch state {
    case "слушаю":
        return "waveform"
    case "речь":
        return "mic.fill"
    case "загрузка":
        return "hourglass"
    default:
        return "waveform.slash"
    }
}

struct Panel: View {
    @EnvironmentObject private var session: Session

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            header
            statusBlock
            commandEditor
            bookmarkEditor
            if !session.note.isEmpty {
                Text(session.note)
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            HStack {
                Button(session.processRunning ? "Остановить" : "Запустить") {
                    if session.processRunning {
                        session.stop()
                    } else {
                        session.start()
                    }
                }
                Button("Сохранить") { session.save() }
                    .keyboardShortcut(.defaultAction)
                Spacer()
                Button("Выйти") {
                    session.stop()
                    NSApp.terminate(nil)
                }
            }
        }
        .padding(16)
        .frame(width: 460)
    }

    private var header: some View {
        HStack {
            Text(session.name)
                .font(.title2)
            Spacer()
            Text(session.state)
                .foregroundStyle(.secondary)
        }
    }

    private var statusBlock: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(session.phrase)
            if !session.command.isEmpty {
                Text(session.command)
                    .foregroundStyle(session.commandOK ? Color.primary : Color.red)
            }
        }
    }

    private var commandEditor: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Команды").font(.headline)
            ForEach($session.commands) { $row in
                HStack {
                    TextField("фраза", text: $row.phrase)
                    Picker("действие", selection: $row.action) {
                        Text("приложение").tag("open_app")
                        Text("браузер").tag("open_browser")
                    }
                    .labelsHidden()
                    .frame(width: 130)
                    TextField("цель", text: $row.target)
                    Button {
                        session.commands.removeAll { $0.token == row.token }
                    } label: {
                        Image(systemName: "minus.circle")
                    }
                    .buttonStyle(.borderless)
                }
            }
            Button("Добавить команду") { session.addCommand() }
        }
    }

    private var bookmarkEditor: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Закладки").font(.headline)
            ForEach($session.bookmarks) { $row in
                HStack {
                    TextField("имя", text: $row.name)
                    TextField("адрес", text: $row.url)
                    Button {
                        session.bookmarks.removeAll { $0.token == row.token }
                    } label: {
                        Image(systemName: "minus.circle")
                    }
                    .buttonStyle(.borderless)
                }
            }
            Button("Добавить закладку") { session.addBookmark() }
        }
    }
}

extension Session {
    var processRunning: Bool { state != "остановлен" }
}
