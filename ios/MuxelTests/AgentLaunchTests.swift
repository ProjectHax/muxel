import XCTest
@testable import muxel

/// Mirrors the agent.rs launch-resolution tests, so the iOS command matches what
/// desktop would run (injection modes, arg ordering, session-resume progression).
final class AgentLaunchTests: XCTestCase {
    func testCtlTaskPreservesSeparateInstructionTransportAcrossWriteBack() throws {
        var original = inst(args: ["--model", "sonnet"], prompt: "/or-spar 20689",
                            injection: .typeIn)
        original.controlPrompt = true
        original.instructionInjection = .cliFlag(flag: "--append-system-prompt")
        original.isRunner = true
        original.autoSubmit = true
        let data = try JSONEncoder().encode(original)
        let restored = try JSONDecoder().decode(Instance.self, from: data)
        XCTAssertEqual(restored, original)
        let json = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(json["control_prompt"] as? Bool, true)
        XCTAssertNotNil(json["instruction_injection"])
        // The phone never owns the task: it neither types nor submits it.
        let launch = AgentLaunch.resolveLaunch(restored)
        XCTAssertEqual(launch.args, original.args)
        XCTAssertNil(launch.startupInput)
        XCTAssertFalse(launch.submit)
    }

    func testPhoneNeverReplaysAnUnconsumedCtlTask() {
        for injection in [InjectionMode.typeIn, .cliFlag(flag: "--append-system-prompt"),
                          InjectionMode.none] {
            var task = inst(args: ["--model", "sonnet"], prompt: "deploy the branch",
                            injection: injection)
            task.controlPrompt = true
            task.instructionInjection = injection
            task.isRunner = true
            task.autoSubmit = true
            let launch = AgentLaunch.resolveLaunch(task)
            XCTAssertEqual(launch.args, ["--model", "sonnet"], "\(injection)")
            XCTAssertNil(launch.startupInput, "\(injection)")
            XCTAssertFalse(launch.submit, "\(injection)")
        }
        // An ordinary runner still types its prompt.
        var runner = inst(prompt: "Review the diff", injection: .typeIn)
        runner.isRunner = true
        runner.autoSubmit = true
        XCTAssertEqual(AgentLaunch.resolveLaunch(runner).startupInput, "Review the diff")
    }

    func testLegacyTaskDefaultsToNoSeparateInstructionTransport() throws {
        let data = Data("""
        {"id":"pane","project_id":"project","title":"task"}
        """.utf8)
        let restored = try JSONDecoder().decode(Instance.self, from: data)
        XCTAssertFalse(restored.controlPrompt)
        XCTAssertNil(restored.instructionInjection)
    }

    func testLiteralLaunchPreservesPolicyAcrossWriteBack() throws {
        var original = inst(args: ["brief with spaces", "", "--"], prompt: "do not inject",
                            injection: .typeIn, sessionId: "old-session", started: true)
        original.literalArgv = true
        original.launchDir = "/srv/external"
        let data = try JSONEncoder().encode(original)
        let restored = try JSONDecoder().decode(Instance.self, from: data)
        XCTAssertEqual(restored, original)
        let json = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(json["literal_argv"] as? Bool, true)
        XCTAssertEqual(json["launch_dir"] as? String, "/srv/external")
        let resolved = AgentLaunch.resolveLaunch(restored)
        XCTAssertEqual(resolved.args, original.args)
        XCTAssertNil(resolved.startupInput)
        XCTAssertFalse(resolved.submit)
        XCTAssertNil(AgentLaunch.builtinPreset(for: restored))
        XCTAssertNil(AgentLaunch.sessionResumeArgs(preset: Preset.builtins[0], instance: restored))
    }

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
