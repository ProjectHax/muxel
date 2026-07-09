import XCTest
@testable import muxel

/// Mirrors the agent.rs launch-resolution tests, so the iOS command matches what
/// desktop would run (injection modes, arg ordering, session-resume progression).
final class AgentLaunchTests: XCTestCase {
    private func inst(program: String? = "claude", args: [String] = [], prompt: String? = nil,
                      injection: InjectionMode = .none, preset: String = "",
                      sessionId: String? = nil, started: Bool = false) -> Instance {
        var i = Instance(id: "aaaa", projectId: "p", title: "t", program: program, args: args)
        i.systemPrompt = prompt
        i.injection = injection
        i.preset = preset
        i.sessionId = sessionId
        i.sessionStarted = started
        return i
    }

    func testCliFlagAppendsFlagAndPrompt() {
        let r = AgentLaunch.resolveLaunch(inst(prompt: "be terse",
                                               injection: .cliFlag(flag: "--append-system-prompt")))
        XCTAssertEqual(r.args, ["--append-system-prompt", "be terse"])
        XCTAssertNil(r.startupInput)
    }

    func testTypeInSetsStartupInput() {
        let r = AgentLaunch.resolveLaunch(inst(prompt: "hello", injection: .typeIn))
        XCTAssertEqual(r.startupInput, "hello")
        XCTAssertTrue(r.args.isEmpty)
    }

    func testEmptyPromptInjectsNothing() {
        let r = AgentLaunch.resolveLaunch(inst(prompt: "", injection: .cliFlag(flag: "-x")))
        XCTAssertTrue(r.args.isEmpty)
        XCTAssertNil(r.startupInput)
    }

    func testNonePromptInjectsNothing() {
        let r = AgentLaunch.resolveLaunch(inst(prompt: "hi", injection: .none))
        XCTAssertTrue(r.args.isEmpty)
        XCTAssertNil(r.startupInput)
    }

    func testComposeArgsOrdersModelEffortExtra() {
        let p = Preset(name: "x", program: "claude", args: ["--extra"],
                       model: "opus", modelFlag: "--model", effort: "high", effortFlag: "--effort")
        XCTAssertEqual(AgentLaunch.composeArgs(p), ["--model", "opus", "--effort", "high", "--extra"])
    }

    func testComposeArgsSkipsUnsetModelAndEffort() {
        let p = Preset(name: "x", program: "claude", args: ["a"], modelFlag: "--model")  // model nil
        XCTAssertEqual(AgentLaunch.composeArgs(p), ["a"])
    }

    func testSessionResumeArgsProgression() {
        let claude = Preset.builtins.first { $0.name == "Claude" }!
        XCTAssertNil(AgentLaunch.sessionResumeArgs(preset: claude, instance: inst(sessionId: nil)))
        XCTAssertEqual(AgentLaunch.sessionResumeArgs(preset: claude, instance: inst(sessionId: "sid", started: false)),
                       ["--session-id", "sid"])
        XCTAssertEqual(AgentLaunch.sessionResumeArgs(preset: claude, instance: inst(sessionId: "sid", started: true)),
                       ["--resume", "sid"])
    }

    func testShellHasNoResume() {
        let shell = Preset.builtins.first { $0.program == nil }!
        XCTAssertNil(AgentLaunch.sessionResumeArgs(preset: shell, instance: inst(program: nil, sessionId: "sid")))
    }

    func testClaudePresetSupportsResume() {
        let claude = Preset.builtins.first { $0.name == "Claude" }!
        XCTAssertEqual(claude.sessionIdFlag, "--session-id")
        XCTAssertEqual(claude.resumeFlag, "--resume")
        if case .cliFlag(let flag) = claude.injection { XCTAssertEqual(flag, "--append-system-prompt") }
        else { XCTFail("Claude should use CliFlag injection") }
    }

    func testBuiltinPresetByNameThenProgram() {
        XCTAssertEqual(AgentLaunch.builtinPreset(for: inst(program: "claude", preset: "Claude"))?.name, "Claude")
        XCTAssertEqual(AgentLaunch.builtinPreset(for: inst(program: "/usr/bin/claude", preset: ""))?.name, "Claude")
        XCTAssertEqual(AgentLaunch.builtinPreset(for: inst(program: nil, preset: ""))?.name, "Shell")
    }
}
