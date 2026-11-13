//! Small Qt reconnect dialog. The main client is SDL/Vulkan and does not link
//! Qt; this helper runs as a child process, uses the desktop's Qt style and
//! exits with 2 when the user cancels.

#include <QApplication>
#include <QCommandLineParser>
#include <QCloseEvent>
#include <QFont>
#include <QGuiApplication>
#include <QHBoxLayout>
#include <QLabel>
#include <QProgressBar>
#include <QPushButton>
#include <QScreen>
#include <QVBoxLayout>
#include <QWidget>

class ReconnectWindow final : public QWidget {
public:
    ReconnectWindow(const QString &server, int attempt) {
        setWindowTitle(QStringLiteral("lgx — reconnecting"));
        setWindowFlag(Qt::WindowStaysOnTopHint, true);
        setAttribute(Qt::WA_DeleteOnClose);

        auto *layout = new QVBoxLayout(this);
        layout->setContentsMargins(20, 18, 20, 14);
        layout->setSpacing(10);

        auto *title = new QLabel(QStringLiteral("Connection lost"), this);
        QFont title_font = title->font();
        title_font.setBold(true);
        title_font.setPointSize(title_font.pointSize() + 2);
        title->setFont(title_font);
        layout->addWidget(title);

        auto *status = new QLabel(
            QStringLiteral("Reconnecting to %1 (attempt %2)…").arg(server).arg(attempt),
            this);
        status->setWordWrap(true);
        layout->addWidget(status);

        auto *bar = new QProgressBar(this);
        bar->setRange(0, 0); // indeterminate "running line"
        bar->setTextVisible(false);
        bar->setMinimumWidth(360);
        layout->addWidget(bar);

        auto *buttons = new QHBoxLayout();
        buttons->addStretch(1);
        auto *cancel = new QPushButton(QStringLiteral("Cancel"), this);
        cancel->setDefault(true);
        buttons->addWidget(cancel);
        layout->addLayout(buttons);

        connect(cancel, &QPushButton::clicked, this, [] {
            QCoreApplication::exit(2);
        });

        resize(440, 160);
        if (QScreen *screen = QGuiApplication::primaryScreen()) {
            move(screen->availableGeometry().center() - rect().center());
        }
    }

protected:
    void closeEvent(QCloseEvent *event) override {
        event->ignore();
        QCoreApplication::exit(2);
    }
};

int main(int argc, char **argv) {
    QApplication app(argc, argv);
    QApplication::setApplicationName(QStringLiteral("lgx-reconnect"));
    QApplication::setApplicationDisplayName(QStringLiteral("lgx"));
    QApplication::setDesktopFileName(QStringLiteral("lgx-client"));

    QCommandLineParser parser;
    parser.setApplicationDescription(QStringLiteral("lgx reconnect dialog"));
    parser.addHelpOption();
    QCommandLineOption server_option(
        QStringList{QStringLiteral("s"), QStringLiteral("server")},
        QStringLiteral("Server address shown in the dialog."),
        QStringLiteral("server"));
    QCommandLineOption attempt_option(
        QStringList{QStringLiteral("a"), QStringLiteral("attempt")},
        QStringLiteral("Reconnect attempt number."),
        QStringLiteral("attempt"),
        QStringLiteral("1"));
    parser.addOptions({server_option, attempt_option});
    parser.process(app);

    ReconnectWindow window(parser.value(server_option), parser.value(attempt_option).toInt());
    window.show();
    return app.exec();
}
