// SPDX-License-Identifier: GPL-2.0-or-later
package com.trexx.sunburst

import android.app.Activity
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Color
import android.graphics.drawable.GradientDrawable
import android.os.Bundle
import android.util.LruCache
import android.view.Gravity
import android.view.KeyEvent
import android.view.View
import android.view.ViewGroup
import android.widget.AdapterView
import android.widget.BaseAdapter
import android.widget.GridView
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import java.util.concurrent.Executors
import kotlin.concurrent.thread

/**
 * The launcher screen: the server's apps as a D-pad grid of box art. Choosing
 * one launches it on the server, then streams; the first tile, Desktop, streams
 * without launching anything. Menu opens settings; an unpaired device goes to
 * pairing first.
 *
 * The catalogue and the launch are Rust (`launcher.rs`), over the same paired
 * control channel the stream uses. Launching *before* the stream connects is
 * what lets a per-app codec override apply: the server reads the running app
 * when the stream's `Hello` arrives.
 *
 * Plain framework views and a programmatic layout, like the other screens: no
 * leanback or androidx dependency for one grid.
 */
class LauncherActivity : Activity() {
    private lateinit var grid: GridView
    private lateinit var status: TextView
    private var tiles: List<AppTile> = listOf(AppTile.DESKTOP)
    private val decoder = Executors.newSingleThreadExecutor()
    /** Decoded box art by file path; the files are named by content, so a path
     *  never changes meaning. Sized by bytes, a quarter of the app's heap. */
    private val bitmaps = object : LruCache<String, Bitmap>(
        (Runtime.getRuntime().maxMemory() / 4).toInt(),
    ) {
        override fun sizeOf(key: String, value: Bitmap) = value.byteCount
    }
    private var busy = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val density = resources.displayMetrics.density
        val tileW = (TILE_W_DP * density).toInt()

        status = TextView(this).apply {
            textSize = 16f
            setTextColor(Color.LTGRAY)
        }
        grid = GridView(this).apply {
            columnWidth = tileW + (SPACING_DP * density).toInt()
            numColumns = GridView.AUTO_FIT
            stretchMode = GridView.STRETCH_SPACING_UNIFORM
            verticalSpacing = (SPACING_DP * density).toInt()
            clipToPadding = false
            setPadding(0, (16 * density).toInt(), 0, (16 * density).toInt())
            // The focused tile is outlined, drawn over the art.
            setDrawSelectorOnTop(true)
            selector = GradientDrawable().apply {
                setStroke((4 * density).toInt(), Color.WHITE)
                cornerRadius = 8 * density
                setColor(Color.TRANSPARENT)
            }
            onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
                private var raised: View? = null
                override fun onItemSelected(p: AdapterView<*>?, v: View?, pos: Int, id: Long) {
                    raised?.animate()?.scaleX(1f)?.scaleY(1f)?.setDuration(120)?.start()
                    v?.animate()?.scaleX(1.08f)?.scaleY(1.08f)?.setDuration(120)?.start()
                    raised = v
                }
                override fun onNothingSelected(p: AdapterView<*>?) {}
            }
            setOnItemClickListener { _, _, position, _ -> choose(tiles[position]) }
        }
        grid.adapter = TileAdapter(tileW)

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding((48 * density).toInt(), (32 * density).toInt(), (48 * density).toInt(), 0)
            setBackgroundColor(Color.rgb(16, 16, 20))
        }
        root.addView(TextView(this).apply {
            text = "Sunburst"
            textSize = 28f
            setTextColor(Color.WHITE)
        })
        root.addView(status)
        root.addView(grid, LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.MATCH_PARENT,
        ))
        setContentView(root)
        grid.requestFocus()
    }

    override fun onResume() {
        super.onResume()
        val prefs = getSharedPreferences("sunburst", MODE_PRIVATE)
        val secret = prefs.getString("secret_hex", "")!!
        if (secret.isEmpty()) {
            startActivity(Intent(this, PairActivity::class.java))
            return
        }
        refresh()
    }

    override fun onDestroy() {
        super.onDestroy()
        decoder.shutdownNow()
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        if (keyCode == KeyEvent.KEYCODE_MENU) {
            startActivity(Intent(this, SettingsActivity::class.java))
            return true
        }
        return super.onKeyDown(keyCode, event)
    }

    private fun server(): Triple<String, Int, String> {
        val prefs = getSharedPreferences("sunburst", MODE_PRIVATE)
        return Triple(
            prefs.getString("server_host", "192.168.1.10")!!,
            prefs.getInt("server_port", 47811),
            prefs.getString("secret_hex", "")!!,
        )
    }

    /** Fetch the catalogue (and any art not yet cached) off the UI thread. */
    private fun refresh() {
        if (busy) return
        busy = true
        status.text = "Loading apps…"
        val (host, port, secret) = server()
        val cache = cacheDir.resolve("art").path
        thread {
            val flat = nativeCatalogue(host, port, secret, cache)
            runOnUiThread {
                busy = false
                tiles = Catalogue.parse(flat)
                status.text = when {
                    flat == null -> "Could not reach the server; Desktop still streams."
                    tiles.size == 1 -> "No apps configured on the server."
                    else -> ""
                }
                (grid.adapter as BaseAdapter).notifyDataSetChanged()
            }
        }
    }

    /** Launch the app (unless Desktop), then stream. */
    private fun choose(tile: AppTile) {
        if (busy) return
        if (tile.isDesktop) {
            stream()
            return
        }
        busy = true
        status.text = "Launching ${tile.name}…"
        val (host, port, secret) = server()
        thread {
            val error = nativeLaunch(host, port, secret, tile.id)
            runOnUiThread {
                busy = false
                status.text = ""
                when {
                    error.isEmpty() -> stream()
                    Catalogue.streamAnyway(error) -> {
                        Toast.makeText(this, error, Toast.LENGTH_LONG).show()
                        stream()
                    }
                    else -> Toast.makeText(this, "Could not launch: $error", Toast.LENGTH_LONG).show()
                }
            }
        }
    }

    private fun stream() = startActivity(Intent(this, StreamActivity::class.java))

    /** Box art tiles: a 2:3 image with the name beneath. */
    private inner class TileAdapter(private val tileW: Int) : BaseAdapter() {
        private val tileH = tileW * 3 / 2

        override fun getCount() = tiles.size
        override fun getItem(position: Int) = tiles[position]
        override fun getItemId(position: Int) = tiles[position].id.toLong()

        override fun getView(position: Int, convertView: View?, parent: ViewGroup?): View {
            val cell = (convertView as? LinearLayout) ?: LinearLayout(this@LauncherActivity).apply {
                orientation = LinearLayout.VERTICAL
                gravity = Gravity.CENTER_HORIZONTAL
                addView(ImageView(context).apply {
                    scaleType = ImageView.ScaleType.CENTER_CROP
                    background = GradientDrawable().apply {
                        setColor(Color.rgb(40, 40, 48))
                        cornerRadius = 8 * resources.displayMetrics.density
                    }
                    clipToOutline = true
                }, LinearLayout.LayoutParams(tileW, tileH))
                addView(TextView(context).apply {
                    setTextColor(Color.WHITE)
                    textSize = 14f
                    maxLines = 2
                    gravity = Gravity.CENTER_HORIZONTAL
                }, LinearLayout.LayoutParams(tileW, ViewGroup.LayoutParams.WRAP_CONTENT))
            }
            val tile = tiles[position]
            val image = cell.getChildAt(0) as ImageView
            (cell.getChildAt(1) as TextView).text = tile.name
            image.tag = tile.artPath
            image.setImageBitmap(null)
            val path = tile.artPath ?: return cell
            val cached = bitmaps.get(path)
            if (cached != null) {
                image.setImageBitmap(cached)
                return cell
            }
            // Decode off the UI thread, no larger than the tile needs.
            decoder.execute {
                val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
                BitmapFactory.decodeFile(path, bounds)
                val options = BitmapFactory.Options().apply {
                    inSampleSize = Catalogue.sampleSize(bounds.outWidth, bounds.outHeight, tileW, tileH)
                }
                val bitmap = BitmapFactory.decodeFile(path, options) ?: return@execute
                runOnUiThread {
                    bitmaps.put(path, bitmap)
                    // The cell may have been reused for another tile by now.
                    if (image.tag == path) image.setImageBitmap(bitmap)
                }
            }
            return cell
        }
    }

    private external fun nativeCatalogue(host: String, port: Int, secretHex: String, cacheDir: String): Array<String>?
    private external fun nativeLaunch(host: String, port: Int, secretHex: String, appId: Int): String

    companion object {
        private const val TILE_W_DP = 160
        private const val SPACING_DP = 24

        init {
            System.loadLibrary("sunburst_android")
        }
    }
}
