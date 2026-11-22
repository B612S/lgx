//! Small Qt dialog shown when the server's capture mode changes. The main
//! client is SDL/Vulkan and does not link Qt; this helper runs as a child
//! process and exits with 0 (change), 1 (change and save) or 2 (cancel).

#include <QApplication>
#include <QCommandLineOption>
#include <QCommandLineParser>
#include <QMessageBox>
#include <QPushButton>
#include <QString>

int main(int argc, char **argv) {
    QApplication app(argc, argv);
    app.setApplicationName(QStringLiteral("lgx"));

    QCommandLineParser parser;
    parser.setApplicationDescription(QStringLiteral("lgx display mode change"));
    parser.addHelpOption();
    QCommandLineOption name_opt(QStringLiteral("name"), QStringLiteral("Profile name"), QStringLiteral("name"));
    QCommandLineOption old_opt(QStringLiteral("old"), QStringLiteral("Previous mode"), QStringLiteral("mode"));
    QCommandLineOption new_opt(QStringLiteral("new"), QStringLiteral("New mode"), QStringLiteral("mode"));
    parser.addOptions({name_opt, old_opt, new_opt});
    parser.process(app);

    const QString name = parser.value(name_opt);
    const QString old_mode = parser.value(old_opt);
    const QString new_mode = parser.value(new_opt);

    QMessageBox box;
    box.setWindowTitle(name.isEmpty() ? QStringLiteral("lgx")
                                      : QStringLiteral("lgx — %1").arg(name));
    box.setWindowFlag(Qt::WindowStaysOnTopHint, true);
    box.setIcon(QMessageBox::Question);
    box.setText(QStringLiteral("Display parameters changed"));
    box.setInformativeText(
        QStringLiteral("%1 → %2\nResize the window to the new mode?")
            .arg(old_mode, new_mode));
    QPushButton *change =
        box.addButton(QStringLiteral("Change"), QMessageBox::AcceptRole);
    QPushButton *save =
        box.addButton(QStringLiteral("Change and save"), QMessageBox::ApplyRole);
    box.addButton(QStringLiteral("Cancel"), QMessageBox::RejectRole);
    box.setDefaultButton(change);
    // Without a parent the Qt Wayland plugin can pick a tiny size hint that
    // clips the question and the buttons.
    box.setMinimumWidth(520);
    box.setSizeGripEnabled(false);
    box.exec();

    if (box.clickedButton() == change) return 0;
    if (box.clickedButton() == save) return 1;
    return 2;
}
