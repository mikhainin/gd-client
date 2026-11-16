#pragma once

#include "gdrive-ui/src/application.h"
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QQmlApplicationEngine>
#include <QQuickItem>
#include <QQuickWindow>
#include <QSaveFile>
#include <QTimer>

inline void smokeTexts(QQuickItem* item, QJsonArray& texts)
{
    if (!item->isVisible())
        return;
    const auto text = item->property("text");
    if (text.isValid() && !text.toString().isEmpty())
        texts.append(text.toString());
    for (auto* child : item->childItems())
        smokeTexts(child, texts);
}

inline void observeSmoke(QQmlApplicationEngine& engine, const QString& path)
{
    // Exercise QWidget construction in the real main process, after loading
    // production QML. QGuiApplication alone aborts here (the startup regression).
    const bool widgetMenu = verifyTrayMenu();
    auto* timer = new QTimer(&engine);
    QObject::connect(timer, &QTimer::timeout, &engine, [&engine, path, widgetMenu, pickerPhase = 0, pickerCancelled = false]() mutable {
        QJsonArray texts;
        QJsonArray tooltips;
        for (auto* root : engine.rootObjects()) {
            if (auto* window = qobject_cast<QQuickWindow*>(root))
                smokeTexts(window->contentItem(), texts);
        }
        bool platformMenu = false;
        for (auto* widget : QApplication::topLevelWidgets())
            platformMenu |= qobject_cast<QMenu*>(widget) != nullptr;
        QJsonObject snapshot {
            {QStringLiteral("rootCount"), engine.rootObjects().size()},
            {QStringLiteral("widgetMenu"), widgetMenu},
            {QStringLiteral("platformWidgetMenu"), platformMenu},
            {QStringLiteral("texts"), texts},
        };
        for (auto* root : engine.rootObjects()) {
            QObject* backend = nullptr;
            for (auto* object : root->findChildren<QObject*>()) {
                if (object->metaObject()->indexOfProperty("tooltip") >= 0)
                    tooltips.append(object->property("tooltip").toString());
                if (object->metaObject()->indexOfProperty("foldersJson") >= 0) {
                    backend = object;
                    snapshot.insert(QStringLiteral("syncState"), object->property("syncState").toString());
                    snapshot.insert(QStringLiteral("syncError"), object->property("syncError").toString());
                }
            }
            auto* add = root->findChild<QObject*>(QStringLiteral("addFolderDialog"));
            if (!add || !backend)
                qFatal("Unable to find production Add dialog/backend");
            if (pickerPhase == 0) {
                if (!QMetaObject::invokeMethod(add, "open")
                    || !backend->setProperty("localPickerLoading", true))
                    qFatal("Unable to prepare Add picker cancellation regression");
                pickerPhase = 1;
            } else if (pickerPhase == 1) {
                if (!QMetaObject::invokeMethod(add, "close"))
                    qFatal("Unable to close production Add dialog");
                pickerPhase = 2;
            } else if (pickerPhase == 2 && !add->property("visible").toBool()
                && !backend->property("localPickerLoading").toBool()) {
                pickerCancelled = true;
                pickerPhase = 3;
            }
            snapshot.insert(QStringLiteral("tooltips"), tooltips);
        }
        snapshot.insert(QStringLiteral("pickerCancelledOnAddClose"), pickerCancelled);
        QSaveFile file(path);
        if (!file.open(QIODevice::WriteOnly)
            || file.write(QJsonDocument(snapshot).toJson()) < 0 || !file.commit())
            qFatal("Unable to publish smoke UI snapshot");
    });
    timer->start(50);
}
