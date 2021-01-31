/*
 *  nextpnr -- Next Generation Place and Route
 *
 *  Copyright (C) 2018  David Shah <david@symbioticeda.com>
 *  Copyright (C) 2021  William D. Jones <wjones@wdj-consulting.com>
 *
 *  Permission to use, copy, modify, and/or distribute this software for any
 *  purpose with or without fee is hereby granted, provided that the above
 *  copyright notice and this permission notice appear in all copies.
 *
 *  THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 *  WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 *  MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
 *  ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 *  WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 *  ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 *  OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 *
 */

#include <fstream>

#include "bitstream.h"
#include "config.h"
#include "nextpnr.h"
#include "util.h"

NEXTPNR_NAMESPACE_BEGIN

// These seem simple enough to do inline for now.
namespace BaseConfigs {
    void config_empty_lcmxo2_1200hc(ChipConfig &cc)
    {
        cc.chip_name = "LCMXO2-1200HC";

        cc.tiles["EBR_R6C11:EBR1"].add_unknown(0, 12);
        cc.tiles["EBR_R6C15:EBR1"].add_unknown(0, 12);
        cc.tiles["EBR_R6C18:EBR1"].add_unknown(0, 12);
        cc.tiles["EBR_R6C21:EBR1"].add_unknown(0, 12);
        cc.tiles["EBR_R6C2:EBR1"].add_unknown(0, 12);
        cc.tiles["EBR_R6C5:EBR1"].add_unknown(0, 12);
        cc.tiles["EBR_R6C8:EBR1"].add_unknown(0, 12);

        cc.tiles["PT4:CFG0"].add_unknown(5, 30);
        cc.tiles["PT4:CFG0"].add_unknown(5, 32);
        cc.tiles["PT4:CFG0"].add_unknown(5, 36);

        cc.tiles["PT7:CFG3"].add_unknown(5, 18);
    }
} // namespace BaseConfigs

// Convert an absolute wire name to a relative Trellis one
static std::string get_trellis_wirename(Context *ctx, Location loc, WireId wire)
{
    std::string basename = ctx->tileInfo(wire)->wire_data[wire.index].name.get();
    std::string prefix2 = basename.substr(0, 2);
    std::string prefix7 = basename.substr(0, 7);
    if (prefix2 == "G_" || prefix2 == "L_" || prefix2 == "R_" || prefix2 == "U_" || prefix2 == "D_" || prefix7 == "BRANCH_")
        return basename;
    if (loc == wire.location)
        return basename;
    std::string rel_prefix;
    if (wire.location.y < loc.y)
        rel_prefix += "N" + std::to_string(loc.y - wire.location.y);
    if (wire.location.y > loc.y)
        rel_prefix += "S" + std::to_string(wire.location.y - loc.y);
    if (wire.location.x > loc.x)
        rel_prefix += "E" + std::to_string(wire.location.x - loc.x);
    if (wire.location.x < loc.x)
        rel_prefix += "W" + std::to_string(loc.x - wire.location.x);
    return rel_prefix + "_" + basename;
}

static void set_pip(Context *ctx, ChipConfig &cc, PipId pip)
{
    std::string tile = ctx->getPipTilename(pip);
    std::string source = get_trellis_wirename(ctx, pip.location, ctx->getPipSrcWire(pip));
    std::string sink = get_trellis_wirename(ctx, pip.location, ctx->getPipDstWire(pip));
    cc.tiles[tile].add_arc(sink, source);
}

static std::vector<bool> int_to_bitvector(int val, int size)
{
    std::vector<bool> bv;
    for (int i = 0; i < size; i++) {
        bv.push_back((val & (1 << i)) != 0);
    }
    return bv;
}

static std::vector<bool> str_to_bitvector(std::string str, int size)
{
    std::vector<bool> bv;
    bv.resize(size, 0);
    if (str.substr(0, 2) != "0b")
        log_error("error parsing value '%s', expected 0b prefix\n", str.c_str());
    for (int i = 0; i < int(str.size()) - 2; i++) {
        char c = str.at((str.size() - i) - 1);
        NPNR_ASSERT(c == '0' || c == '1');
        bv.at(i) = (c == '1');
    }
    return bv;
}

std::string intstr_or_default(const std::unordered_map<IdString, Property> &ct, const IdString &key,
                              std::string def = "0")
{
    auto found = ct.find(key);
    if (found == ct.end())
        return def;
    else {
        if (found->second.is_string)
            return found->second.as_string();
        else
            return std::to_string(found->second.as_int64());
    }
};

void write_bitstream(Context *ctx, std::string text_config_file)
{
    ChipConfig cc;

    switch (ctx->args.type) {
    case ArchArgs::LCMXO2_1200HC:
        BaseConfigs::config_empty_lcmxo2_1200hc(cc);
        break;
    default:
        NPNR_ASSERT_FALSE("Unsupported device type");
    }

    cc.metadata.push_back("Part: " + ctx->getFullChipName());

    // Add all set, configurable pips to the config
    for (auto pip : ctx->getPips()) {
        if (ctx->getBoundPipNet(pip) != nullptr) {
            if (ctx->getPipClass(pip) == 0) { // ignore fixed pips
                set_pip(ctx, cc, pip);
            }
        }
    }

    // TODO: Bank Voltages

    // Configure slices
    for (auto &cell : ctx->cells) {
        CellInfo *ci = cell.second.get();
        if (ci->bel == BelId()) {
            log_warning("found unplaced cell '%s' during bitstream gen\n", ci->name.c_str(ctx));
        }
        BelId bel = ci->bel;
        if (ci->type == id_FACADE_SLICE) {
            std::string tname = ctx->getTileByTypeAndLocation(bel.location.y, bel.location.x, "PLC");
            std::string slice = ctx->tileInfo(bel)->bel_data[bel.index].name.get();

            NPNR_ASSERT(slice.substr(0, 5) == "SLICE");
            int int_index = slice[5] - 'A';
            NPNR_ASSERT(int_index >= 0 && int_index < 4);

            int lut0_init = int_or_default(ci->params, ctx->id("LUT0_INITVAL"));
            int lut1_init = int_or_default(ci->params, ctx->id("LUT1_INITVAL"));
            cc.tiles[tname].add_word(slice + ".K0.INIT", int_to_bitvector(lut0_init, 16));
            cc.tiles[tname].add_word(slice + ".K1.INIT", int_to_bitvector(lut1_init, 16));
            cc.tiles[tname].add_enum(slice + ".MODE", str_or_default(ci->params, ctx->id("MODE"), "LOGIC"));
            cc.tiles[tname].add_enum(slice + ".GSR", str_or_default(ci->params, ctx->id("GSR"), "ENABLED"));
            cc.tiles[tname].add_enum("LSR" + std::to_string(int_index) + ".SRMODE", str_or_default(ci->params, ctx->id("SRMODE"), "LSR_OVER_CE"));
            cc.tiles[tname].add_enum(slice + ".CEMUX", intstr_or_default(ci->params, ctx->id("CEMUX"), "1"));
            cc.tiles[tname].add_enum("CLK" + std::to_string(int_index) + ".CLKMUX", intstr_or_default(ci->params, ctx->id("CLKMUX"), "0"));
            cc.tiles[tname].add_enum("LSR" + std::to_string(int_index) + ".LSRMUX", str_or_default(ci->params, ctx->id("LSRMUX"), "LSR"));
            cc.tiles[tname].add_enum("LSR" + std::to_string(int_index) + ".LSRONMUX", intstr_or_default(ci->params, ctx->id("LSRONMUX"), "LSRMUX"));
            cc.tiles[tname].add_enum(slice + ".REGMODE", str_or_default(ci->params, ctx->id("REGMODE"), "FF"));
            cc.tiles[tname].add_enum(slice + ".REG0.SD", intstr_or_default(ci->params, ctx->id("REG0_SD"), "0"));
            cc.tiles[tname].add_enum(slice + ".REG1.SD", intstr_or_default(ci->params, ctx->id("REG1_SD"), "0"));
            cc.tiles[tname].add_enum(slice + ".REG0.REGSET",
                                     str_or_default(ci->params, ctx->id("REG0_REGSET"), "RESET"));
            cc.tiles[tname].add_enum(slice + ".REG1.REGSET",
                                     str_or_default(ci->params, ctx->id("REG1_REGSET"), "RESET"));
        }
    }

    // Configure chip
    if (!text_config_file.empty()) {
        std::ofstream out_config(text_config_file);
        out_config << cc;
    }
}

NEXTPNR_NAMESPACE_END
