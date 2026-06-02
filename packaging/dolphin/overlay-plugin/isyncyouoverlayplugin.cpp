/*
    iSyncYou Dolphin overlay-icon plugin.
    SPDX-License-Identifier: Apache-2.0
*/
#include "isyncyouoverlayplugin.h"

#include <QDBusConnection>
#include <QDBusMessage>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QDateTime>
#include <QUrl>

namespace
{
constexpr char kService[] = "org.silentspike.iSyncYou";
constexpr char kPath[] = "/org/silentspike/iSyncYou/FileStatus";
constexpr char kIface[] = "org.silentspike.iSyncYou.FileStatus";

// How long a cached answer is reused before re-querying (ms). Keeps getOverlays()
// cheap during a repaint storm while still picking up status changes on refresh.
constexpr qint64 kTtlMs = 5000;

// Map the daemon's status string onto Breeze emblem icon names.
QStringList emblemsFor(const QString &status)
{
    if (status == QLatin1String("synced")) {
        return {QStringLiteral("emblem-checked")};
    }
    if (status == QLatin1String("syncing")) {
        return {QStringLiteral("view-refresh")};
    }
    if (status == QLatin1String("error")) {
        return {QStringLiteral("emblem-error")};
    }
    if (status == QLatin1String("ignored")) {
        return {QStringLiteral("emblem-unavailable")};
    }
    return {}; // unknown / untracked -> no overlay
}
}

ISyncYouOverlayPlugin::ISyncYouOverlayPlugin(QObject *parent)
    : KOverlayIconPlugin(parent)
{
}

QStringList ISyncYouOverlayPlugin::getOverlays(const QUrl &item)
{
    if (!item.isLocalFile()) {
        return {};
    }
    const QString path = item.toLocalFile();
    const qint64 now = QDateTime::currentMSecsSinceEpoch();

    auto it = m_cache.constFind(path);
    if (it != m_cache.constEnd()) {
        if (now - it.value().fetchedAtMs < kTtlMs) {
            return it.value().overlays; // fresh
        }
        // Stale: refresh in the background, but return the last value now (no flicker).
        queryAsync(item, path);
        return it.value().overlays;
    }

    // Unknown: kick off an async query (must not block here) and answer empty.
    queryAsync(item, path);
    return {};
}

void ISyncYouOverlayPlugin::queryAsync(const QUrl &url, const QString &path)
{
    QDBusMessage msg = QDBusMessage::createMethodCall(QString::fromLatin1(kService),
                                                      QString::fromLatin1(kPath),
                                                      QString::fromLatin1(kIface),
                                                      QStringLiteral("Status"));
    msg << path;
    const QDBusPendingCall call = QDBusConnection::sessionBus().asyncCall(msg);
    auto *watcher = new QDBusPendingCallWatcher(call, this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, url, path](QDBusPendingCallWatcher *w) {
        const QDBusPendingReply<QString> reply = *w;
        w->deleteLater();
        if (reply.isError()) {
            // Daemon not running / no answer: drop any entry so a later repaint retries.
            m_cache.remove(path);
            return;
        }
        const QStringList overlays = emblemsFor(reply.value());
        m_cache.insert(path, Entry{overlays, QDateTime::currentMSecsSinceEpoch()});
        Q_EMIT overlaysChanged(url, overlays);
    });
}
