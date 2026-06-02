#
# spec file for package isyncyou-dolphin
#
# The native host package for iSyncYou's KDE/Dolphin integration: the overlay-icon
# plugin (KOverlayIconPlugin) and the right-click ServiceMenu. This part must be a
# host package — KIO loads the overlay plugin into Dolphin's own process, so it
# cannot ship inside the app's Flatpak/AppImage. The distro build service compiles
# the plugin against the exact KF6 the distro ships, so users just install it.
#

Name:           isyncyou-dolphin
Version:        0.1.0
Release:        0
Summary:        Dolphin integration for iSyncYou (overlay icons + service menu)
License:        Apache-2.0
URL:            https://github.com/silentspike/isyncyou
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  cmake
BuildRequires:  gcc-c++
BuildRequires:  extra-cmake-modules
BuildRequires:  kf6-kio-devel
BuildRequires:  kf6-kcoreaddons-devel
BuildRequires:  qt6-base-devel
# Pulls qtpaths6 so KDE_INSTALL_USE_QT_SYS_PATHS resolves the system Qt plugin dir.
BuildRequires:  qt6-base-common-devel

# The ServiceMenu actions call the iSyncYou CLI / status window from the main app.
Requires:       isyncyou
Requires:       kf6-kio

%description
A KDE Frameworks 6 overlay-icon plugin (KIO KOverlayIconPlugin) that paints a
sync-status emblem on files and folders in Dolphin by querying the running
iSyncYou daemon over DBus, plus a right-click ServiceMenu. Overlays are advisory:
if the daemon is not running no overlay is shown and the ServiceMenu remains the
fallback.

%prep
%autosetup -n %{name}-%{version}

%build
%cmake -DKDE_INSTALL_USE_QT_SYS_PATHS=ON
%cmake_build

%install
%cmake_install

%post
# KDE rebuilds its service cache automatically on next session start; nothing to
# do as root here (kbuildsycoca6 is per-user/session).

%files
%license LICENSE
%{_kf6_plugindir}/kf6/overlayicon/isyncyouoverlay.so
%{_datadir}/kio/servicemenus/org.silentspike.iSyncYou.desktop

%changelog
