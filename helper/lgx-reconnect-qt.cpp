//! Small Qt reconnect dialog. The main client is SDL/Vulkan and does not link
//! Qt; this helper runs as a child process, uses the desktop's Qt style and
//! exits with 2 when the user cancels.

#include <QApplication>
#include <QByteArray>
#include <QCommandLineParser>
#include <QCloseEvent>
#include <QFont>
#include <QGuiApplication>
#include <QHBoxLayout>
#include <QLabel>
#include <QProgressBar>
#include <QPushButton>
#include <QScreen>
#include <QSocketNotifier>
#include <QVBoxLayout>
#include <QWidget>

#include <csignal>
#include <memory>
#include <sys/prctl.h>
#include <unistd.h>

class ReconnectWindow final : public QWidget {
public:
    ReconnectWindow(const QString &name, int attempt) : name_(name) {
        setWindowTitle(QStringLiteral("lgx — %1").arg(name_));
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

        status_ = new QLabel(this);
        status_->setWordWrap(true);
        layout->addWidget(status_);
        set_attempt(attempt);

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

public:
    void set_attempt(int attempt) {
        status_->setText(
            QStringLiteral("Reconnecting to %1 (attempt %2)…").arg(name_).arg(attempt));
    }

protected:
    void closeEvent(QCloseEvent *event) override {
        event->ignore();
        QCoreApplication::exit(2);
    }

private:
    QString name_;
    QLabel *status_ = nullptr;
};

int main(int argc, char **argv) {
    // Do not leave an orphan dialog if the client process dies.
    prctl(PR_SET_PDEATHSIG, SIGTERM);
    QApplication app(argc, argv);
    QApplication::setApplicationName(QStringLiteral("lgx"));
    QApplication::setDesktopFileName(QStringLiteral("lgx-client"));

    QCommandLineParser parser;
    parser.setApplicationDescription(QStringLiteral("lgx reconnect dialog"));
    parser.addHelpOption();
    QCommandLineOption name_option(
        QStringList{QStringLiteral("n"), QStringLiteral("name")},
        QStringLiteral("Profile name shown in the dialog."),
        QStringLiteral("name"),
        QStringLiteral("lgx"));
    QCommandLineOption attempt_option(
        QStringList{QStringLiteral("a"), QStringLiteral("attempt")},
        QStringLiteral("Reconnect attempt number."),
        QStringLiteral("attempt"),
        QStringLiteral("1"));
    parser.addOptions({name_option, attempt_option});
    parser.process(app);

    ReconnectWindow window(parser.value(name_option), parser.value(attempt_option).toInt());
    window.show();

    // The parent updates the attempt number over stdin; no polling and no
    // process restart for every backoff step.
    auto buffer = std::make_shared<QByteArray>();
    auto *notifier = new QSocketNotifier(STDIN_FILENO, QSocketNotifier::Read, &window);
    QObject::connect(notifier, &QSocketNotifier::activated, notifier,
                     [notifier, buffer, &window] {
        char data[256];
        const ssize_t n = ::read(STDIN_FILENO, data, sizeof(data));
        if (n <= 0) {
            notifier->setEnabled(false);
            return;
        }
        buffer->append(data, static_cast<int>(n));
        for (;;) {
            const int nl = buffer->indexOf('\n');
            if (nl < 0) {
                return;
            }
            const QString line = QString::fromUtf8(buffer->left(nl)).trimmed();
            buffer->remove(0, nl + 1);
            if (line.startsWith(QStringLiteral("attempt "))) {
                bool ok = false;
                const int attempt = line.mid(8).toInt(&ok);
                if (ok) {
                    window.set_attempt(attempt);
                }
            }
        }
    });

    return app.exec();
}
