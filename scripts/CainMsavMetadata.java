import arc.files.Fi;
import arc.struct.StringMap;
import mindustry.io.MapIO;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

public class CainMsavMetadata {
    public static void main(String[] args) throws Exception {
        if (args.length == 0) {
            System.err.println("Usage: CainMsavMetadata <file.msav>");
            System.exit(2);
            return;
        }

        mindustry.maps.Map map = MapIO.createMap(new Fi(args[0]), true);
        StringBuilder out = new StringBuilder();
        out.append('{');
        appendField(out, "file_name", map.file == null ? "" : map.file.name(), true);
        appendField(out, "file_path", map.file == null ? "" : map.file.absolutePath(), true);
        appendField(out, "name", map.tags.get("name", ""), true);
        appendField(out, "author", map.tags.get("author", ""), true);
        appendField(out, "description", map.tags.get("description", ""), true);
        appendNumber(out, "width", map.width, true);
        appendNumber(out, "height", map.height, true);
        appendNumber(out, "version", map.version, true);
        appendNumber(out, "build", map.build, true);
        appendField(out, "rules_json", map.tags.get("rules", ""), true);
        appendField(out, "suggested_mode", detectMode(map.tags), true);
        out.append("\"tags\":{");
        List<String> keys = new ArrayList<>();
        for (Object key : map.tags.keys()) {
            keys.add(String.valueOf(key));
        }
        Collections.sort(keys);
        boolean first = true;
        for (String key : keys) {
            if (!first) {
                out.append(',');
            }
            first = false;
            out.append('"').append(escape(key)).append('"').append(':').append('"').append(escape(map.tags.get(key, ""))).append('"');
        }
        out.append('}');
        out.append('}');
        System.out.print(out);
    }

    static String detectMode(StringMap tags) {
        String rules = tags.get("rules", "");
        String mode = tags.get("gamemode", "");
        if (!mode.isEmpty()) {
            return mode;
        }
        String lower = rules.toLowerCase();
        if (lower.contains("\"pvp\":true") || lower.contains("pvp:true")) {
            return "pvp";
        }
        if (lower.contains("\"attackmode\":true") || lower.contains("attackmode:true")) {
            return "attack";
        }
        if (lower.contains("\"waves\":false") || lower.contains("waves:false")) {
            return "sandbox";
        }
        return "survival";
    }

    static void appendField(StringBuilder out, String key, String value, boolean comma) {
        out.append('"').append(escape(key)).append('"').append(':').append('"').append(escape(value)).append('"');
        if (comma) {
            out.append(',');
        }
    }

    static void appendNumber(StringBuilder out, String key, int value, boolean comma) {
        out.append('"').append(escape(key)).append('"').append(':').append(value);
        if (comma) {
            out.append(',');
        }
    }

    static String escape(String value) {
        if (value == null) {
            return "";
        }
        StringBuilder out = new StringBuilder();
        for (int index = 0; index < value.length(); index += 1) {
            char c = value.charAt(index);
            switch (c) {
                case '\\' -> out.append("\\\\");
                case '"' -> out.append("\\\"");
                case '\n' -> out.append("\\n");
                case '\r' -> out.append("\\r");
                case '\t' -> out.append("\\t");
                default -> {
                    if (c < 32) {
                        out.append(String.format("\\u%04x", (int)c));
                    } else {
                        out.append(c);
                    }
                }
            }
        }
        return out.toString();
    }
}
