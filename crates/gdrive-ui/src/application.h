#pragma once

#include "cxx-qt-lib/qcoreapplication.h"
#include <QApplication>
#include <QMenu>
#include <memory>

inline std::unique_ptr<QGuiApplication> createWidgetApplication(const QVector<QByteArray>& args)
{
    auto storage = std::make_unique<rust::cxxqtlib1::ApplicationArgsData>(args);
    auto app = std::make_unique<QApplication>(storage->size(), storage->data());
    storage.release()->setParent(app.get());
    return app;
}

inline bool verifyTrayMenu()
{
    QMenu menu;
    auto* action = menu.addAction(QStringLiteral("Quit"));
    bool triggered = false;
    QObject::connect(action, &QAction::triggered, [&triggered] { triggered = true; });
    action->trigger();
    return triggered;
}
