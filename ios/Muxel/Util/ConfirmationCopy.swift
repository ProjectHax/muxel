import Foundation

/// Copy for the destructive-delete confirmation dialogs. Pure so the
/// pluralization / reference counting is unit-testable; every message spells out
/// what is device-local vs what (nothing) happens on the remote.
enum ConfirmationCopy {
    static func deleteHost(_ host: Host, projectCount: Int) -> (title: String, message: String) {
        let projects = projectCount == 1 ? "its project" : "its \(projectCount) projects"
        let message = projectCount == 0
            ? "Removes this host and its saved credentials from this device, and "
                + "disconnects its live terminals. Nothing on the remote is touched."
            : "Removes \(projects) and this host's saved credentials from this device, "
                + "and disconnects its live terminals. Nothing on the remote is touched."
        return (title: "Delete \(host.name)?", message: message)
    }

    static func deleteProjects(_ projects: [RemoteProject]) -> (title: String, message: String) {
        let title = projects.count == 1
            ? "Remove \(projects[0].name)?"
            : "Remove \(projects.count) projects?"
        let it = projects.count == 1 ? "it" : "them"
        return (title: title,
                message: "Removes \(it) from this device only — sessions on the host keep running.")
    }

    static func deleteIdentity(_ identity: Identity, hostCount: Int) -> (title: String, message: String) {
        let message: String
        switch hostCount {
        case 0:
            message = "No hosts use this login. Its Keychain secret is deleted."
        case 1:
            message = "1 host uses this login and will fall back to its own credentials. "
                + "Its Keychain secret is deleted."
        default:
            message = "\(hostCount) hosts use this login and will fall back to their own "
                + "credentials. Its Keychain secret is deleted."
        }
        return (title: "Delete \(identity.name)?", message: message)
    }
}
