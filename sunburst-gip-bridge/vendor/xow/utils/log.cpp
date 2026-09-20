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

#include "log.h"
#include "bytes.h"

#include <cstdio>

// NB: deliberately no <sstream>/<iomanip>/<ostream> here. This is the only C++ module in
// the project and APP_STL is c++_static, so anything that touches iostreams statically
// links libc++'s locale machinery into libxow-driver.so - which measured at roughly
// 800 KB per ABI. The formatting below is snprintf-based for that reason. Keep it that
// way: reintroducing a std::ostringstream anywhere in this driver brings all of it back.

namespace Log
{
    std::string formatBytes(const Bytes &bytes)
    {
        if (bytes.size() == 0)
        {
            return std::string();
        }

        // Two hex digits per byte plus a separating colon between each pair
        std::string output;
        output.resize(bytes.size() * 3 - 1);

        char *out = &output[0];
        size_t remaining = output.size() + 1;

        for (size_t i = 0; i < bytes.size(); i++)
        {
            // Every byte but the last is followed by a colon
            int written = snprintf(out, remaining,
                i + 1 < bytes.size() ? "%02x:" : "%02x",
                static_cast<uint32_t>(bytes.raw()[i]));

            if (written <= 0 || static_cast<size_t>(written) >= remaining)
            {
                break;
            }

            out += written;
            remaining -= written;
        }

        return output;
    }
}
