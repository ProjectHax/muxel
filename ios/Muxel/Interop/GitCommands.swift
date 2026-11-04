import Foundation

/// Read-only Git invocations shared by remote viewers and repository probes.
enum GitCommands {
    static func readOnly(_ args: [String]) -> String {
        "GIT_OPTIONAL_LOCKS=0 git -c diff.autoRefreshIndex=false "
            + args.map(Shell.quote).joined(separator: " ")
    }
}
