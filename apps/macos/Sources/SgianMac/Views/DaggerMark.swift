import SwiftUI

struct DaggerMark: View {
    var size: CGFloat = 22

    var body: some View {
        ZStack {
            RoundedRectangle(cornerRadius: size * 0.13, style: .continuous)
                .fill(.primary)
                .frame(width: size * 0.18, height: size * 0.68)
                .rotationEffect(.degrees(38))
                .offset(x: size * 0.02, y: -size * 0.04)
            Capsule()
                .fill(.primary)
                .frame(width: size * 0.62, height: size * 0.14)
                .rotationEffect(.degrees(38))
                .offset(x: -size * 0.1, y: size * 0.12)
            DaggerBlade()
                .fill(.primary)
                .frame(width: size * 0.27, height: size * 0.58)
                .rotationEffect(.degrees(38))
                .offset(x: -size * 0.15, y: size * 0.18)
        }
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }
}

private struct DaggerBlade: Shape {
    func path(in rect: CGRect) -> Path {
        var path = Path()
        path.move(to: CGPoint(x: rect.midX, y: rect.maxY))
        path.addLine(to: CGPoint(x: rect.minX, y: rect.minY))
        path.addLine(to: CGPoint(x: rect.maxX, y: rect.minY))
        path.closeSubpath()
        return path
    }
}
