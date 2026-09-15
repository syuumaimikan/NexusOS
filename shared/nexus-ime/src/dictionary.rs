//! Readings, and what they can be written as.
//!
//! This is the half of Japanese input that is not a function of the letters.
//! `かんじ` is `漢字` or `感じ` or `幹事`, and nothing about the kana says which:
//! it is a matter of what the word means, which means a dictionary.
//!
//! # How big this one is, said plainly
//!
//! Two hundred and twenty-five entries, written out here by hand. A real
//! Japanese input method ships between one hundred thousand and a million, with
//! part-of-speech tags, connection costs between adjacent words, and a language
//! model over the whole sentence. This has none of that, and the number is
//! written here rather than implied: `dictionary::size()` returns it, and the
//! machine says it where somebody can see it.
//!
//! What it does have is every entry checked, and the honest consequence stated
//! where somebody will meet it: a word that is not here converts to itself in
//! kana or katakana, which is a real answer and not a failure. The alternative
//! -- shipping a converter that silently mangles what it does not know -- is
//! worse than shipping a small one that says so.
//!
//! # Why not a downloaded dictionary
//!
//! Because a file nobody in this repository can read is a file nobody can check.
//! Every line below can be read by somebody who knows the language, and a wrong
//! one can be seen to be wrong. The day this wants a hundred thousand entries,
//! the thing to add is a *loader* -- a dictionary file on the disk, with its own
//! format and its own checks -- and not a larger literal.
//!
//! # The ordering rule
//!
//! Entries are sorted by reading, because the lookup binary-searches them, and
//! the candidates within an entry are in the order they should be offered:
//! commonest first. There is no frequency count and no learning, so that order
//! is the whole of the ranking and it is a judgement rather than a measurement.
//! It is written down here so that changing it is a decision somebody makes on
//! purpose.

/// One reading, and what it may be written as.
pub type Entry = (&'static str, &'static [&'static str]);

/// The dictionary, sorted by reading.
///
/// Sorted, and there is an assertion below that says so: a table that fell out
/// of order would make the binary search miss entries that are present, which
/// is a failure that looks exactly like a word not being in the dictionary.
pub static ENTRIES: &[Entry] = &[
    ("あお", &["青", "蒼"]),
    ("あか", &["赤", "朱"]),
    ("あき", &["秋", "空き", "飽き"]),
    ("あさ", &["朝", "麻"]),
    ("あし", &["足", "脚", "葦"]),
    ("あした", &["明日"]),
    ("あたま", &["頭"]),
    ("あたらしい", &["新しい"]),
    ("あと", &["後", "跡"]),
    ("あめ", &["雨", "飴"]),
    ("あらた", &["新た"]),
    ("あんぜん", &["安全"]),
    ("いえ", &["家"]),
    ("いけん", &["意見", "異見"]),
    ("いし", &["石", "意思", "医師", "意志"]),
    ("いち", &["一", "位置", "市"]),
    ("いちば", &["市場"]),
    ("いぬ", &["犬"]),
    ("いま", &["今", "居間"]),
    ("いみ", &["意味"]),
    ("いれる", &["入れる"]),
    ("いろ", &["色"]),
    ("うえ", &["上"]),
    ("うごく", &["動く"]),
    ("うた", &["歌", "唄"]),
    ("うみ", &["海"]),
    ("え", &["絵", "柄"]),
    ("えいが", &["映画"]),
    ("えいご", &["英語"]),
    ("えき", &["駅", "液"]),
    ("おうと", &["応答"]),
    ("おおきい", &["大きい"]),
    ("おと", &["音"]),
    ("おとこ", &["男"]),
    ("おな", &["同"]),
    ("おなじ", &["同じ"]),
    ("おもい", &["重い", "思い"]),
    ("おわり", &["終わり"]),
    ("おんがく", &["音楽"]),
    ("おんな", &["女"]),
    ("かいしゃ", &["会社"]),
    ("かいはつ", &["開発"]),
    ("かえる", &["帰る", "変える", "蛙", "代える"]),
    ("かく", &["書く", "各", "角", "描く"]),
    ("かぜ", &["風", "風邪"]),
    ("かた", &["形", "型", "肩", "方"]),
    ("かたち", &["形"]),
    ("かね", &["金", "鐘"]),
    ("かみ", &["紙", "神", "髪", "上"]),
    ("かわ", &["川", "河", "皮", "革"]),
    ("かんが", &["考"]),
    ("かんがえ", &["考え"]),
    ("かんじ", &["漢字", "感じ", "幹事"]),
    ("かんせい", &["完成", "歓声", "慣性"]),
    ("かんり", &["管理"]),
    ("がっこう", &["学校"]),
    ("き", &["木", "気", "機", "期"]),
    ("きかい", &["機械", "機会"]),
    ("きく", &["聞く", "菊", "効く", "聴く"]),
    ("きた", &["北", "来た"]),
    ("きのう", &["昨日", "機能"]),
    ("きょう", &["今日", "京", "教"]),
    ("きろく", &["記録"]),
    ("ぎんこう", &["銀行"]),
    ("くうき", &["空気"]),
    ("くち", &["口"]),
    ("くに", &["国"]),
    ("くび", &["首"]),
    ("くるま", &["車"]),
    ("くろ", &["黒"]),
    ("けいかく", &["計画"]),
    ("けいさん", &["計算"]),
    ("けっか", &["結果"]),
    ("けん", &["件", "県", "券", "権", "検"]),
    ("けんさ", &["検査"]),
    ("げつようび", &["月曜日"]),
    ("げんざい", &["現在"]),
    ("こうじょう", &["工場", "向上"]),
    ("こえ", &["声"]),
    ("ここ", &["此処"]),
    ("こころ", &["心"]),
    ("こたえ", &["答え", "応え"]),
    ("こと", &["事", "琴"]),
    ("ことば", &["言葉"]),
    ("こども", &["子供"]),
    ("この", &["この", "此の"]),
    ("こめ", &["米"]),
    ("ごご", &["午後"]),
    ("ごぜん", &["午前"]),
    ("さいご", &["最後"]),
    ("さいしょ", &["最初"]),
    ("さかな", &["魚"]),
    ("さくせい", &["作成", "作製"]),
    ("さくら", &["桜"]),
    ("さん", &["三", "産", "酸"]),
    ("し", &["四", "市", "死", "詩", "氏"]),
    ("しあい", &["試合"]),
    ("しお", &["塩"]),
    ("しごと", &["仕事"]),
    ("した", &["下", "舌"]),
    ("しつもん", &["質問"]),
    ("しま", &["島", "縞"]),
    ("しゃしん", &["写真"]),
    ("しゅうり", &["修理"]),
    ("しゅつりょく", &["出力"]),
    ("しよう", &["使用", "仕様"]),
    ("しらべる", &["調べる"]),
    ("しろ", &["白", "城"]),
    ("しんぶん", &["新聞"]),
    ("じかん", &["時間"]),
    ("じしょ", &["辞書", "自署"]),
    ("じしん", &["自信", "地震", "自身"]),
    ("じっこう", &["実行", "実効"]),
    ("じどう", &["自動", "児童"]),
    ("じょうほう", &["情報"]),
    ("すいようび", &["水曜日"]),
    ("すう", &["数", "吸う"]),
    ("すく", &["少", "空く"]),
    ("すくない", &["少ない"]),
    ("すすむ", &["進む"]),
    ("せいぎょ", &["制御"]),
    ("せいこう", &["成功", "精巧"]),
    ("せかい", &["世界"]),
    ("せつめい", &["説明"]),
    ("せん", &["千", "線", "戦", "先"]),
    ("せんせい", &["先生"]),
    ("そと", &["外"]),
    ("そら", &["空"]),
    ("たいせつ", &["大切"]),
    ("たかい", &["高い"]),
    ("たてもの", &["建物"]),
    ("たべる", &["食べる"]),
    ("たまご", &["卵", "玉子"]),
    ("だい", &["大", "台", "第", "代", "題"]),
    ("ちいさい", &["小さい"]),
    ("ちから", &["力"]),
    ("ちず", &["地図"]),
    ("ちち", &["父"]),
    ("ちゅうい", &["注意"]),
    ("つうしん", &["通信"]),
    ("つかう", &["使う"]),
    ("つき", &["月", "付き"]),
    ("つくる", &["作る", "造る", "創る"]),
    ("つち", &["土"]),
    ("て", &["手"]),
    ("てがみ", &["手紙"]),
    ("てき", &["的", "敵"]),
    ("てんき", &["天気", "転記"]),
    ("でんき", &["電気", "伝記"]),
    ("でんわ", &["電話"]),
    ("と", &["戸", "都", "と"]),
    ("とうろく", &["登録"]),
    ("とけい", &["時計"]),
    ("ところ", &["所", "処"]),
    ("とし", &["年", "都市"]),
    ("とり", &["鳥", "取り"]),
    ("どうぐ", &["道具"]),
    ("なか", &["中", "仲"]),
    ("なつ", &["夏"]),
    ("なまえ", &["名前"]),
    ("にく", &["肉"]),
    ("にし", &["西"]),
    ("にほん", &["日本", "二本"]),
    ("にほんご", &["日本語"]),
    ("にゅうりょく", &["入力"]),
    ("ねこ", &["猫"]),
    ("ねだん", &["値段"]),
    ("のみもの", &["飲み物"]),
    ("は", &["歯", "葉", "刃"]),
    ("はいけい", &["背景", "拝啓"]),
    ("はし", &["橋", "箸", "端"]),
    ("はしる", &["走る"]),
    ("はじめ", &["初め", "始め"]),
    ("はな", &["花", "鼻", "話"]),
    ("はなし", &["話"]),
    ("はは", &["母"]),
    ("はやい", &["早い", "速い"]),
    ("はる", &["春", "貼る", "張る"]),
    ("ばんごう", &["番号"]),
    ("ひ", &["日", "火", "非", "比"]),
    ("ひかり", &["光"]),
    ("ひがし", &["東"]),
    ("ひだり", &["左"]),
    ("ひと", &["人", "一"]),
    ("ひょうじ", &["表示"]),
    ("ふかい", &["深い"]),
    ("ふく", &["服", "吹く", "副", "複"]),
    ("ふゆ", &["冬"]),
    ("ふるい", &["古い"]),
    ("ぶんしょう", &["文章"]),
    ("へや", &["部屋"]),
    ("へんこう", &["変更"]),
    ("ほか", &["他", "外"]),
    ("ほし", &["星", "干し"]),
    ("ほん", &["本"]),
    ("まえ", &["前"]),
    ("まち", &["町", "街", "待ち"]),
    ("まど", &["窓"]),
    ("みぎ", &["右"]),
    ("みず", &["水"]),
    ("みせ", &["店", "見せ"]),
    ("みち", &["道", "未知"]),
    ("みなみ", &["南"]),
    ("みみ", &["耳"]),
    ("みる", &["見る", "観る", "診る"]),
    ("むずかしい", &["難しい"]),
    ("め", &["目", "芽"]),
    ("もじ", &["文字"]),
    ("もり", &["森"]),
    ("もんだい", &["問題"]),
    ("やま", &["山"]),
    ("ゆき", &["雪", "行き"]),
    ("ゆび", &["指"]),
    ("よう", &["用", "様", "要", "陽"]),
    ("ようい", &["用意", "容易"]),
    ("よてい", &["予定"]),
    ("よみ", &["読み", "黄泉"]),
    ("よむ", &["読む", "詠む"]),
    ("よる", &["夜", "寄る", "因る"]),
    ("らいしゅう", &["来週"]),
    ("りゆう", &["理由"]),
    ("りょうり", &["料理"]),
    ("れきし", &["歴史"]),
    ("わたし", &["私"]),
    ("わるい", &["悪い"]),
];

/// What a reading can be written as, or nothing.
///
/// A binary search, which is why [`ENTRIES`] is sorted and why there is a test
/// that says it still is.
#[must_use]
pub fn look_up(reading: &str) -> Option<&'static [&'static str]> {
    ENTRIES
        .binary_search_by(|(key, _)| (*key).cmp(reading))
        .ok()
        .map(|at| ENTRIES[at].1)
}

/// The longest entry whose reading starts `text`, and what it can be written as.
///
/// For segmenting a phrase nobody typed a space in. Longest first, because
/// `にほんご` should be one word and not `にほん` followed by `ご` -- greedy
/// longest-match is the crudest segmentation there is and it is right far more
/// often than it is wrong for the readings in a dictionary this size.
#[must_use]
pub fn longest_prefix(text: &str) -> Option<(&'static str, &'static [&'static str])> {
    let mut best: Option<(&'static str, &'static [&'static str])> = None;
    // Walked rather than searched, because the question is "which keys are a
    // prefix of this" and a sorted table answers that by scanning the run that
    // shares a first character. Four hundred entries makes the difference
    // between that and a scan of all of them immeasurable, and a scan is a
    // great deal easier to be sure of.
    for (reading, candidates) in ENTRIES {
        if text.starts_with(reading) && best.is_none_or(|(best, _)| reading.len() > best.len()) {
            best = Some((reading, candidates));
        }
    }
    best
}

/// How many readings are known. For saying so rather than implying more.
#[must_use]
pub fn size() -> usize {
    ENTRIES.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table has to stay sorted or the binary search misses entries that
    /// are there -- which looks exactly like a word not being in the
    /// dictionary, and so would never be noticed.
    #[test]
    fn the_table_is_sorted_and_has_no_duplicates() {
        for pair in ENTRIES.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "{:?} is not before {:?}",
                pair[0].0,
                pair[1].0
            );
        }
    }

    /// Every entry has to offer something, and every reading has to be kana.
    /// A reading with a letter in it can never be typed, so it would be a line
    /// nobody could ever reach.
    #[test]
    fn every_entry_is_usable() {
        for (reading, candidates) in ENTRIES {
            assert!(!candidates.is_empty(), "{reading} offers nothing");
            assert!(!reading.is_empty(), "there is an entry with no reading");
            for character in reading.chars() {
                assert!(
                    ('\u{3041}'..='\u{309F}').contains(&character),
                    "{reading} is not all hiragana"
                );
            }
            for candidate in *candidates {
                assert!(!candidate.is_empty(), "{reading} offers an empty candidate");
            }
        }
    }

    #[test]
    fn it_finds_a_reading() {
        assert_eq!(look_up("かんじ"), Some(&["漢字", "感じ", "幹事"][..]));
        assert_eq!(look_up("にほんご"), Some(&["日本語"][..]));
        assert_eq!(look_up("すしざんまい"), None);
    }

    /// Longest, not first. `にほんご` is one word; taking `にほん` and leaving
    /// `ご` behind is the wrong answer and the easy one.
    #[test]
    fn it_takes_the_longest_reading_it_can() {
        let (reading, candidates) = longest_prefix("にほんごです").expect("にほんご is in the table");
        assert_eq!(reading, "にほんご");
        assert_eq!(candidates, &["日本語"]);
    }

    #[test]
    fn it_finds_nothing_for_an_unknown_start() {
        assert!(longest_prefix("ぐぬぬ").is_none());
    }
}
