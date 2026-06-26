cmake_minimum_required(VERSION 3.16)
project(isyncyou-dolphin VERSION 1.0.0 LANGUAGES CXX)

# Top-level build for the iSyncYou Dolphin integration. Builds the overlay-icon
# plugin and installs it together with the right-click ServiceMenu to the system
# KDE paths — this is what the distro/host package (see isyncyou-dolphin.spec)
# uses for its %build / %install, so end users install a package and never compile.

# Tell ECM/KDEInstallDirs we target Qt6 *before* including it, so resolving the
# system Qt plugin path (KDE_INSTALL_USE_QT_SYS_PATHS) queries qtpaths6, not Qt5.
set(QT_MAJOR_VERSION 6)

find_package(ECM REQUIRED NO_MODULE)
set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})
include(KDEInstallDirs)

# Overlay-icon plugin (its own CMakeLists builds + installs the .so MODULE).
add_subdirectory(overlay-plugin)

# Right-click ServiceMenu: context-menu actions -> the iSyncYou CLI / status window.
# KF6 reads service menus from <datadir>/kio/servicemenus/.
install(FILES org.silentspike.iSyncYou.desktop
        DESTINATION ${KDE_INSTALL_DATADIR}/kio/servicemenus)
