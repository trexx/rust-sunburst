/*
 * Copyright (C) 2019 Medusalix
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License
 * as published by the Free Software Foundation; either version 2
 * of the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program; if not, write to the Free Software
 * Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA  02110-1301, USA.
 */

#pragma once

#include <string>
#include <android/log.h>

class Bytes;
#define APPNAME "xow-driver"

#define PRImac "%02x:%02x:%02x:%02x:%02x:%02x"
#define VALmac(arr) arr[0],arr[1],arr[2],arr[3],arr[4],arr[5]

#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wformat-security"

/*
 * Provides logging functions for different log levels.
 *
 * Debug logging exists only in debug builds: _DEBUG comes from Android.mk under NDK_DEBUG=1, and
 * without it the calls below have empty bodies, so the compiler discards both the call and its
 * format string rather than emitting a line nobody reads. Info and error are unconditional.
 *
 * Because the release bodies are empty rather than absent, arguments are still evaluated. That
 * costs nothing for a literal or a scalar, which is all but one of the call sites; anything that
 * builds a string to pass in needs guarding at the call site instead. See Dongle::handleControllerPair().
 */
namespace Log
{
    std::string formatBytes(const Bytes &bytes);

    inline void init()
    {
        // Switch to line buffering
        setlinebuf(stdout);
    }

#ifdef _DEBUG
    inline void debug(const char *message)
    {
        __android_log_print(ANDROID_LOG_DEBUG, APPNAME, message);
    }

    template<typename... Args>
    inline void debug(const char * message, Args... args)
    {
        __android_log_print(ANDROID_LOG_DEBUG, APPNAME, message, args...);
    }
#else
    inline void debug(const char *) {}

    template<typename... Args>
    inline void debug(const char *, Args...) {}
#endif

    inline void info(const char * message)
    {
        __android_log_print(ANDROID_LOG_INFO, APPNAME, message);
    }

    template<typename... Args>
    inline void info(const char * message, Args... args)
    {
        __android_log_print(ANDROID_LOG_INFO, APPNAME, message, args...);
    }

    inline void error(const char * message)
    {
        __android_log_print(ANDROID_LOG_ERROR, APPNAME, message);
    }

    template<typename... Args>
    inline void error(const char * message, Args... args)
    {
        __android_log_print(ANDROID_LOG_ERROR, APPNAME, message, args...);
    }
}

#pragma GCC diagnostic pop

