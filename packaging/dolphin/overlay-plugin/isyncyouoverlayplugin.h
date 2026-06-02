/*
    iSyncYou Dolphin overlay-icon plugin.
    SPDX-License-Identifier: Apache-2.0
*/
#pragma once

#include <KOverlayIconPlugin>
#include <QHash>
#include <QStringList>

/**
 * Paints a sync-status emblem on files/folders in Dolphin by asking the running
 * iSyncYou daemon over DBus (service org.silentspike.iSyncYou, see the Rust
 * `isyncyou-dbus-status` crate).
 *
 * KOverlayIconPlugin::getOverlays() is called on the GUI thread and must not
 * block, so this plugin answers from a short-TTL cache and issues an asynchronous
 * DBus query; when the reply arrives it updates the cache and emits
 * overlaysChanged() so Dolphin repaints. If the daemon is not running, no overlay
 * is shown (graceful — overlays are advisory).
 */
class ISyncYouOverlayPlugin : public KOverlayIconPlugin
{
    Q_OBJECT
    Q_PLUGIN_METADATA(IID "org.kde.overlayicon.isyncyou")

public:
    explicit ISyncYouOverlayPlugin(QObject *parent = nullptr);

    QStringList getOverlays(const QUrl &item) override;

private:
    struct Entry {
        QStringList overlays;
        qint64 fetchedAtMs = 0;
    };

    void queryAsync(const QUrl &url, const QString &path);

    QHash<QString, Entry> m_cache; // local path -> last known overlays + timestamp
};
