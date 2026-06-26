cmake_minimum_required(VERSION 3.16)
project(isyncyou-dolphin-overlay VERSION 1.0.0 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)

# Extra CMake Modules + the standard KDE settings. KDECMakeSettings sets
# BUILD_SHARED_LIBS ON and the library output directory that
# kcoreaddons_add_plugin needs to build a shared MODULE (.so) plugin.
# Target Qt6 before the KDE modules so the system Qt path query uses qtpaths6.
set(QT_MAJOR_VERSION 6)

find_package(ECM REQUIRED NO_MODULE)
set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})
include(KDEInstallDirs)
include(KDECMakeSettings)

# kcoreaddons_add_plugin requires a library output dir; set one if KDECMakeSettings
# did not (its default location varies across ECM versions).
if(NOT CMAKE_LIBRARY_OUTPUT_DIRECTORY)
    set(CMAKE_LIBRARY_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/bin)
endif()

find_package(Qt6 REQUIRED COMPONENTS Core DBus)
# KIOCore declares KOverlayIconPlugin; CoreAddons provides kcoreaddons_add_plugin.
find_package(KF6 REQUIRED COMPONENTS KIO CoreAddons)

# Builds the plugin and installs it into the KF6 overlay-icon plugin namespace,
# where KIO/Dolphin load it automatically.
kcoreaddons_add_plugin(isyncyouoverlay
    SOURCES isyncyouoverlayplugin.cpp isyncyouoverlayplugin.h
    INSTALL_NAMESPACE "kf6/overlayicon")

target_link_libraries(isyncyouoverlay
    Qt6::Core
    Qt6::DBus
    KF6::KIOCore)
