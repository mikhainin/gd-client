#pragma once

#include <QGuiApplication>
#include <QIcon>
#include <QImage>
#include <QPainter>
#include <QWindow>

inline bool configureApplicationIcon()
{
    const QIcon icon(QStringLiteral(":/qt/qml/org/gclient/gdrive_ui/icons/org.gclient.gdrive-ui.svg"));
    QGuiApplication::setWindowIcon(icon);
    return !icon.pixmap(32, 32).isNull();
}

inline bool verifyApplicationIcons(const QString& previewDirectory)
{
    const QString prefix = QStringLiteral(":/qt/qml/org/gclient/gdrive_ui/icons/");
    const QStringList names = {QStringLiteral("org.gclient.gdrive-ui"),
                               QStringLiteral("syncing"),
                               QStringLiteral("error"),
                               QStringLiteral("disconnected")};
    for (const auto& name : names) {
        const QIcon icon(prefix + name + QStringLiteral(".svg"));
        for (const int size : {16, 22, 24, 32, 48, 128}) {
            const auto pixmap = icon.pixmap(size, size);
            if (pixmap.isNull() || pixmap.size() != QSize(size, size))
                return false;
            const auto image = pixmap.toImage();
            bool visible = false;
            for (int y = 0; y < size; ++y)
                for (int x = 0; x < size; ++x)
                    visible = visible || qAlpha(image.pixel(x, y)) != 0;
            if (!visible)
                return false;
            if (!previewDirectory.isEmpty()) {
                for (const auto& background : {QStringLiteral("light"), QStringLiteral("dark")}) {
                    QImage preview(size, size, QImage::Format_ARGB32);
                    preview.fill(background == QStringLiteral("light") ? QColor("#ffffff") : QColor("#232629"));
                    QPainter painter(&preview);
                    painter.drawImage(0, 0, image);
                    painter.end();
                    if (!preview.save(previewDirectory + "/" + name + "-" + QString::number(size) + "-" + background + ".png"))
                        return false;
                }
            }
        }
    }
    QWindow window;
    return !QGuiApplication::windowIcon().pixmap(32, 32).isNull()
        && !window.icon().pixmap(32, 32).isNull();
}
