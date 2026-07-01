import SwiftUI
import WidgetKit

/// The widget extension's entry point. Hosts the muxel Live Activity (the only
/// widget in the bundle; static/home-screen widgets could be added here later).
@main
struct MuxelWidgetsBundle: WidgetBundle {
    var body: some Widget {
        MuxelLiveActivity()
    }
}
