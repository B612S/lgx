//! Transient OSD for lgx background-audio toggles. It is a separate Qt
//! process: the SDL/Vulkan client does not link Qt. The window is click-through,
//! takes no focus and fades itself out.

#include <QApplication>
#include <QCommandLineParser>
#include <QFont>
#include <QGuiApplication>
#include <QLinearGradient>
#include <QPainter>
#include <QPainterPath>
#include <QPropertyAnimation>
#include <QScreen>
#include <QTimer>
#include <QWidget>

#include <csignal>
#include <sys/prctl.h>

class Osd final : public QWidget {
public:
    explicit Osd(bool enabled) : enabled_(enabled) {
        setWindowFlags(Qt::Tool | Qt::FramelessWindowHint | Qt::WindowStaysOnTopHint
                       | Qt::WindowTransparentForInput | Qt::WindowDoesNotAcceptFocus);
        setAttribute(Qt::WA_TranslucentBackground);
        setAttribute(Qt::WA_ShowWithoutActivating);
        setFocusPolicy(Qt::NoFocus);
        resize(360, 84);
        setWindowOpacity(0.0);
    }

    void show_centered() {
        if (QScreen *screen = QGuiApplication::primaryScreen()) {
            const QRect area = screen->availableGeometry();
            move(area.center().x() - width() / 2, area.bottom() - height() - 64);
        }
        show();
        auto *in = new QPropertyAnimation(this, "windowOpacity", this);
        in->setDuration(140);
        in->setStartValue(0.0);
        in->setEndValue(1.0);
        in->start(QAbstractAnimation::DeleteWhenStopped);
        QTimer::singleShot(1350, this, [this] { fade_out(); });
    }

protected:
    void paintEvent(QPaintEvent *) override {
        QPainter p(this);
        p.setRenderHint(QPainter::Antialiasing);
        p.setRenderHint(QPainter::TextAntialiasing);

        const QRectF card = QRectF(rect()).adjusted(8, 8, -8, -8);
        QLinearGradient bg(card.topLeft(), card.bottomRight());
        bg.setColorAt(0.0, QColor(38, 38, 46, 238));
        bg.setColorAt(1.0, QColor(21, 21, 27, 238));
        QPainterPath path;
        path.addRoundedRect(card, 18, 18);
        p.fillPath(path, bg);
        p.setPen(QPen(QColor(255, 255, 255, 28), 1));
        p.drawPath(path);

        const QColor accent = enabled_ ? QColor(126, 229, 158) : QColor(238, 179, 104);
        const qreal left = card.left() + 26;
        const qreal width = card.width() - 52;

        QFont title_font = font();
        title_font.setPointSize(10);
        title_font.setWeight(QFont::DemiBold);
        p.setFont(title_font);
        p.setPen(QColor(247, 247, 250));
        p.drawText(QRectF(left, card.top() + 17, width, 22),
                   Qt::AlignLeft | Qt::AlignVCenter, QStringLiteral("Background audio"));

        QFont state_font = font();
        state_font.setPointSize(9);
        p.setFont(state_font);
        p.setPen(accent);
        p.drawText(QRectF(left, card.top() + 41, width, 20),
                   Qt::AlignLeft | Qt::AlignVCenter,
                   enabled_ ? QStringLiteral("On") : QStringLiteral("Off — muted when unfocused"));
    }

private:
    void fade_out() {
        auto *out = new QPropertyAnimation(this, "windowOpacity", this);
        out->setDuration(220);
        out->setStartValue(windowOpacity());
        out->setEndValue(0.0);
        connect(out, &QPropertyAnimation::finished, qApp, &QCoreApplication::quit);
        out->start(QAbstractAnimation::DeleteWhenStopped);
    }

    bool enabled_;
};

int main(int argc, char **argv) {
    prctl(PR_SET_PDEATHSIG, SIGTERM);
    QApplication app(argc, argv);
    QApplication::setApplicationName(QStringLiteral("lgx"));
    QApplication::setDesktopFileName(QStringLiteral("lgx-client"));

    QCommandLineParser parser;
    parser.addHelpOption();
    QCommandLineOption state_option(
        QStringList{QStringLiteral("s"), QStringLiteral("state")},
        QStringLiteral("on or off."),
        QStringLiteral("state"),
        QStringLiteral("off"));
    parser.addOption(state_option);
    parser.process(app);

    const bool enabled = parser.value(state_option).compare(QStringLiteral("on"), Qt::CaseInsensitive) == 0;
    Osd osd(enabled);
    osd.show_centered();
    return app.exec();
}
