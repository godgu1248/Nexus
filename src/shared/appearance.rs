//! 外观目录：强调色（theme）、背板（backdrop）、字体（font）。
//!
//! 这里是外观选项的**唯一事实来源**：白名单、展示名、默认值都在这一份表里。
//! CSS 只负责按 `body.theme-<key>` / `body.bg-<key>` / `body.font-<key>` 覆盖样式，
//! 模板只负责把 key 拼进 class。三处各写一份列表是这类功能最容易腐烂的地方。
//!
//! 取值范围是**封闭**的：三列都是 VARCHAR(32)，写入前必须过 `normalize_*`，
//! 所以库里不可能出现白名单之外的 key（除了手工改库，那种情况渲染时回落默认值）。

/// 一个可选项。三类外观共用一个结构：它们需要的东西完全一样，
/// 拆成三个相同的结构体只会让校验逻辑写三遍。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Choice {
    /// 存库、拼 class 用的稳定标识（小写 ASCII，永不变）。
    pub key: &'static str,
    /// 中文展示名。
    pub label: &'static str,
    /// 英文名，色卡/选项上的副标题。
    pub en: &'static str,
    /// 一句话说明，选项下面显示。
    pub hint: &'static str,
}

/// 默认强调色。与 `migrations/0008_user_theme.sql` 的 DEFAULT 必须一致。
pub const DEFAULT_THEME: &str = "ink";

/// 默认背板。与 `migrations/0009_user_appearance.sql` 的 DEFAULT 必须一致。
pub const DEFAULT_BACKDROP: &str = "plain";

/// 默认字体。与 `migrations/0009_user_appearance.sql` 的 DEFAULT 必须一致。
pub const DEFAULT_FONT: &str = "sans";

/// 全部强调色。顺序即设置页的展示顺序。
pub const THEMES: &[Choice] = &[
    Choice {
        key: "ink",
        label: "墨蓝",
        en: "Ink",
        hint: "默认。深色底 + 冷蓝强调色，长时间看不累。",
    },
    Choice {
        key: "ocean",
        label: "海青",
        en: "Ocean",
        hint: "偏青的冷静配色，适合白天开着灯用。",
    },
    Choice {
        key: "forest",
        label: "苔绿",
        en: "Forest",
        hint: "低饱和绿，状态标签不刺眼。",
    },
    Choice {
        key: "sunset",
        label: "暮橙",
        en: "Sunset",
        hint: "暖橙强调色，晚上看更柔和。",
    },
    Choice {
        key: "violet",
        label: "雾紫",
        en: "Violet",
        hint: "紫调偏冷，和蓝色区分开又不像换了个站。",
    },
    Choice {
        key: "rose",
        label: "蔷薇",
        en: "Rose",
        hint: "粉红强调色，只换强调色不换底色，克制。",
    },
    Choice {
        key: "amber",
        label: "琥珀",
        en: "Amber",
        hint: "金黄强调色，截图放文档里最醒目。",
    },
    Choice {
        key: "slate",
        label: "石墨",
        en: "Slate",
        hint: "几乎无色相的中性灰，适合把页面投屏。",
    },
];

/// 全部背板。`plain` 是素底；其余五个是极低对比的纹理/光晕，
/// 都在 `::before` 上画，正文永远压在它们上面。
pub const BACKDROPS: &[Choice] = &[
    Choice {
        key: "plain",
        label: "素底",
        en: "Plain",
        hint: "只有底色，最安静。截图不带任何纹理。",
    },
    Choice {
        key: "grain",
        label: "颗粒",
        en: "Grain",
        hint: "极细噪点，深色下像胶片颗粒，凑近才看得见。",
    },
    Choice {
        key: "mesh",
        label: "层云",
        en: "Mesh",
        hint: "右上一团柔光，像房间里有盏台灯。",
    },
    Choice {
        key: "aurora",
        label: "极光",
        en: "Aurora",
        hint: "顶部一条斜向彩带，最显眼的一个。",
    },
    Choice {
        key: "stripes",
        label: "细纹",
        en: "Stripes",
        hint: "45 度极细斜纹，有织物感，不抢文字。",
    },
    Choice {
        key: "grid",
        label: "方格",
        en: "Grid",
        hint: "低对比网格，像稿纸。适合喜欢对齐感的人。",
    },
];

/// 全部字体。全部走系统已装字体栈，不下载 webfont——
/// 2C2G 的机器上多一个字体文件就是多一次慢加载，而且中文字体动辄几 MB。
pub const FONTS: &[Choice] = &[
    Choice {
        key: "sans",
        label: "黑体",
        en: "Sans",
        hint: "系统默认无衬线，紧凑，信息密度最高。",
    },
    Choice {
        key: "serif",
        label: "宋体",
        en: "Serif",
        hint: "衬线体，长文档和档案页更好读。",
    },
    Choice {
        key: "mono",
        label: "等宽",
        en: "Mono",
        hint: "等宽字体，表格、代码块、时间戳对齐得整整齐齐。",
    },
    Choice {
        key: "round",
        label: "圆润",
        en: "Round",
        hint: "优先选系统里的圆体/无衬线细体，标题更轻。",
    },
];

fn find(list: &'static [Choice], key: &str) -> Option<&'static Choice> {
    let trimmed = key.trim();
    list.iter().find(|choice| choice.key == trimmed)
}

/// 这个 key 是否在给定目录里。
pub fn is_known(list: &'static [Choice], key: &str) -> bool {
    find(list, key).is_some()
}

/// 把任意输入收敛成合法 key：合法就原样返回，否则回落到给定默认值。
///
/// 刻意**不**报错：外观是纯偏好，一个存坏的 key 不该让人打不开页面。
/// 需要报错的地方是写入前的校验（`settings_submit`），那里会 422。
pub fn normalize_in(list: &'static [Choice], key: &str, default: &'static str) -> &'static str {
    find(list, key).map(|choice| choice.key).unwrap_or(default)
}

pub fn label_in(list: &'static [Choice], key: &str, default: &'static str) -> &'static str {
    let normalized = normalize_in(list, key, default);
    find(list, normalized)
        .map(|choice| choice.label)
        .unwrap_or(default)
}

pub fn is_valid_theme(key: &str) -> bool {
    is_known(THEMES, key)
}

pub fn is_valid_backdrop(key: &str) -> bool {
    is_known(BACKDROPS, key)
}

pub fn is_valid_font(key: &str) -> bool {
    is_known(FONTS, key)
}

pub fn normalize_theme(key: &str) -> &'static str {
    normalize_in(THEMES, key, DEFAULT_THEME)
}

pub fn normalize_backdrop(key: &str) -> &'static str {
    normalize_in(BACKDROPS, key, DEFAULT_BACKDROP)
}

pub fn normalize_font(key: &str) -> &'static str {
    normalize_in(FONTS, key, DEFAULT_FONT)
}

/// 一次渲染需要的三项外观，全部已 normalize。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Appearance {
    pub theme: &'static str,
    pub backdrop: &'static str,
    pub font: &'static str,
}

impl Appearance {
    pub const DEFAULT: Self = Self {
        theme: DEFAULT_THEME,
        backdrop: DEFAULT_BACKDROP,
        font: DEFAULT_FONT,
    };

    pub fn normalize(theme: &str, backdrop: &str, font: &str) -> Self {
        Self {
            theme: normalize_theme(theme),
            backdrop: normalize_backdrop(backdrop),
            font: normalize_font(font),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalogs() -> [(&'static str, &'static [Choice], &'static str); 3] {
        [
            ("theme", THEMES, DEFAULT_THEME),
            ("backdrop", BACKDROPS, DEFAULT_BACKDROP),
            ("font", FONTS, DEFAULT_FONT),
        ]
    }

    #[test]
    fn keys_are_unique_ascii_lowercase_and_documented() {
        for (name, list, _) in catalogs() {
            for (i, choice) in list.iter().enumerate() {
                assert!(
                    choice
                        .key
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c == '-'),
                    "{name} 的 key 必须是小写 ASCII：{}",
                    choice.key
                );
                assert!(
                    !choice.label.is_empty() && !choice.en.is_empty() && !choice.hint.is_empty(),
                    "{name} 的 {} 展示文案不能为空",
                    choice.key
                );
                assert!(
                    list.iter().skip(i + 1).all(|other| other.key != choice.key),
                    "{name} 的 key 重复：{}",
                    choice.key
                );
            }
        }
    }

    #[test]
    fn defaults_are_in_their_catalogs() {
        assert!(is_valid_theme(DEFAULT_THEME));
        assert!(is_valid_backdrop(DEFAULT_BACKDROP));
        assert!(is_valid_font(DEFAULT_FONT));
    }

    #[test]
    fn unknown_keys_fall_back_instead_of_panicking() {
        assert_eq!(normalize_theme("nope"), DEFAULT_THEME);
        assert_eq!(normalize_theme(""), DEFAULT_THEME);
        assert_eq!(normalize_theme("  ocean  "), "ocean");
        assert_eq!(normalize_backdrop("nope"), DEFAULT_BACKDROP);
        assert_eq!(normalize_font("nope"), DEFAULT_FONT);
        assert_eq!(
            Appearance::normalize("nope", "nope", "nope"),
            Appearance::DEFAULT
        );
    }

    #[test]
    fn valid_keys_survive_normalisation() {
        let look = Appearance::normalize("  violet ", " aurora", "mono ");
        assert_eq!(look.theme, "violet");
        assert_eq!(look.backdrop, "aurora");
        assert_eq!(look.font, "mono");
    }

    #[test]
    fn catalogs_are_big_enough_to_be_a_palette() {
        // owner 明确要求"各种颜色/调色盘/背景/字体"都给选项，这里把数量钉住，
        // 免得以后被"顺手精简"到两三个。
        assert!(THEMES.len() >= 6, "强调色太少，调色盘就不成其为调色盘");
        assert!(BACKDROPS.len() >= 4, "背板选项太少");
        assert!(FONTS.len() >= 3, "字体选项太少");
    }
}
