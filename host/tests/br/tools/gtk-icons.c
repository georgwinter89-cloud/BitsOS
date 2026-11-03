/* BR-7 (40-download.sh, host step): GTK 3 looks up and draws the icons its file chooser uses from the staged
 * tree's Adwaita, through gdk-pixbuf's glycin loaders named in the tree's configs and the tree's MIME database, in
 * a clean environment as the launcher gives Brave. Prints one line per icon; exit 0 when every one loads. */
#include <gtk/gtk.h>

int main(void) {
    static const char *names[] = {"folder-symbolic", "user-home-symbolic", "folder-download-symbolic",
        "document-save-symbolic", "pan-down-symbolic", "edit-find-symbolic", "folder", "text-x-generic",
        "image-x-generic", "image-missing"};
    GtkIconTheme *t = gtk_icon_theme_new();
    gtk_icon_theme_set_custom_theme(t, "Adwaita");
    int bad = 0;
    for (unsigned i = 0; i < G_N_ELEMENTS(names); i++) {
        GError *e = NULL;
        GtkIconInfo *ii = gtk_icon_theme_lookup_icon(t, names[i], 16, 0);
        if (!ii) {
            printf("%s: not in the theme\n", names[i]);
            bad = 1;
            continue;
        }
        GdkRGBA fg = {0, 0, 0, 1};
        GdkPixbuf *p = gtk_icon_info_is_symbolic(ii) ? gtk_icon_info_load_symbolic(ii, &fg, NULL, NULL, NULL, NULL, &e)
                                                     : gtk_icon_info_load_icon(ii, &e);
        printf("%s: %s %s\n", names[i], gtk_icon_info_get_filename(ii), p ? "loaded" : e->message);
        bad |= p == NULL;
        g_clear_error(&e);
        g_clear_object(&p);
        g_object_unref(ii);
    }
    return bad;
}
