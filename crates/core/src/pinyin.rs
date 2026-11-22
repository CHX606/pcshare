//! 轻量模糊匹配与常用汉字拼音首字母匹配（任务 3.1、3.9，零外部依赖，L1 缓存常驻平面查表 + 零堆分配快路径）。

use std::sync::OnceLock;

const CJK_BASE: usize = 0x4E00;
const CJK_END: usize = 0x9FA5;
const CJK_LEN: usize = CJK_END - CJK_BASE + 1;

struct PinyinTable {
    cjk_lut: Box<[u8; CJK_LEN]>,
    extras: Vec<(char, u8)>,
}

static PINYIN_TABLE: OnceLock<PinyinTable> = OnceLock::new();

const BUCKETS: &[(char, &str)] = &[
    ('a', "阿啊哎哀埃挨癌矮艾爱碍安氨俺岸按案暗昂凹熬傲奥澳"),
    ('b', "八巴扒吧芭疤捌拔跋把靶坝爸罢霸白百柏摆败拜班般颁斑搬板版办半伴扮拌绊帮绑榜膀傍棒包胞宝饱保堡报抱豹暴爆杯悲碑北贝备背倍被辈奔本笨崩逼鼻比鄙笔彼币必毕闭辟碧蔽壁避臂边编鞭扁便变遍辨辩标彪膘表憋别宾滨冰兵丙柄饼并病拨波玻剥播脖伯驳泊勃博搏膊薄卜补捕不布步簿部"),
    ('c', "擦猜裁材才财睬踩采彩菜蔡参餐残蚕惨灿仓苍舱藏操糙槽草册侧厕测层蹭插叉茶查察擦拆柴豺搀掺蝉馋缠产铲阐颤昌猖长肠尝偿常厂场敞畅倡唱抄钞超巢朝嘲潮吵炒车扯彻掣撤臣辰尘晨衬称撑成呈承诚城乘惩程澄橙逞骋吃痴池驰迟持匙尺齿耻斥赤翅充冲虫崇抽仇绸愁稠筹酬丑臭出初除厨锄础储楚处触川穿传船喘串疮窗床闯创吹炊锤春纯唇醇蠢戳词祠雌瓷慈辞磁赐次刺匆葱囱从丛凑粗促醋簇窜篡催摧脆粹翠村存寸搓撮挫措错"),
    ('d', "搭达答打大呆歹代带待怠殆贷袋逮戴丹单担耽胆旦但诞弹淡蛋氮当挡党荡档刀导岛倒蹈到悼盗道稻得德的灯登等瞪凳低堤滴迪敌涤笛底抵地弟帝递第颠典点碘电店垫惦淀殿刁叼雕吊钓调掉爹跌叠碟蝶丁叮盯钉顶鼎订定丢东冬懂动冻栋洞兜抖斗陡豆逗督毒读独堵赌睹妒度渡端短段断缎堆队对兑吨敦蹲盾顿多夺朵躲剁舵惰跺"),
    ('e', "讹俄鹅额恶饿鳄恩儿而尔耳饵二贰"),
    ('f', "发乏伐罚阀筏法帆番翻凡烦繁反返犯泛饭范贩方坊芳防妨房肪仿访纺放飞非啡菲肥匪诽吠废沸肺费分吩纷芬坟焚粉份奋愤粪丰风枫封疯峰锋蜂逢缝讽凤奉佛否夫肤孵敷弗伏扶拂服俘浮符幅福抚府斧俯釜辅腐父付妇负附咐阜复赴副傅富赋腹覆"),
    ('g', "该改钙盖溉干甘杆肝柑竿秆赶敢感赣冈刚钢缸纲岗港杠高膏羔糕搞稿告哥歌搁割鸽胳阁革格葛隔个各给根跟更耕羹埂耿梗工弓公功攻供宫恭躬巩汞拱共贡勾沟钩狗苟构购够辜菇咕箍估沽孤姑古谷股骨蛊鼓固故顾雇刮瓜剐寡挂褂乖拐怪棺关官冠观管馆罐惯灌贯光广逛归圭龟规闺轨鬼诡癸桂柜贵跪辊滚棍锅郭国果裹过"),
    ('h', "哈骸孩海氦亥害骇酣憨含函寒韩罕喊汉汗旱悍焊瀚航毫豪嚎壕好号浩耗呵喝禾合何和河核荷盒禾贺褐赫鹤黑嘿痕很狠恨哼恒横衡轰哄烘虹鸿洪宏弘红喉猴吼后厚候乎呼忽狐胡壶湖糊蝴虎唬互户护沪花华哗滑猾化划画话槐怀淮坏欢环桓还缓换患唤痪豢煌皇黄惶煌晃谎灰挥辉徽恢蛔悔卉汇会讳绘诲贿晦秽惠毁慧昏婚浑魂混豁活火伙或货获祸惑霍"),
    ('j', "击饥圾机肌鸡迹姬积基绩稽畸箕讥及吉级即极急疾集籍几己挤脊计记纪伎忌际妓季剂济既继寄寂加夹佳家嘉荚颊甲钾假稼价驾架嫁奸尖坚间肩艰兼监笺渐浆僵疆讲奖桨匠降酱蕉椒礁焦胶交郊浇骄娇嚼搅铰矫侥脚角饺缴绞剿教酵轿较叫窖揭接皆秸街阶截劫节桔杰捷睫竭洁结解姐戒藉芥界借介疥诫届巾筋斤金今津襟紧锦仅谨进靳晋禁近烬浸尽劲京经茎惊晶睛精鲸井警景颈静境敬镜径痉靖竟竞净炯窘揪究纠玖韭久灸九酒厩救旧臼舅咎就疚鞠拘狙疽居驹菊局咀矩举沮聚拒据巨具距踞锯俱句惧炬剧捐鹃娟倦眷卷绢撅攫抉掘倔爵觉决诀绝均菌钧军君峻俊竣浚郡骏"),
    ('k', "咖卡咯开揩楷凯慨刊堪勘坎砍看康慷糠扛抗亢炕考拷烤靠坷苛柯棵磕颗科壳咳可渴克刻客课肯啃垦恳坑吭空恐孔控抠口扣寇枯哭窟苦酷库裤夸垮挎跨胯块筷侩快宽款匡筐狂框矿眶旷况亏盔岿窥葵奎魁傀馈愧溃坤昆捆困括扩廓阔"),
    ('l', "垃拉啦喇叭腊蜡辣来莱赖蓝婪栏拦篮阑兰澜谰揽览懒缆烂滥琅榔狼廊郎朗浪捞劳牢老佬姥酪烙涝勒乐雷镭蕾磊累儡垒擂肋类泪棱楞冷厘梨犁黎篱狸离漓理李里鲤礼莉荔吏栗丽厉励砾历利傈例俐痢立粒沥隶力璃哩俩联莲连镰廉怜涟帘敛脸链恋炼练粮凉梁粱良两辆量晾亮谅撩聊僚疗燎寥辽潦了撂镣廖料列裂烈劣猎琳林磷霖临邻鳞淋凛赁吝拎玲菱零龄铃伶羚凌灵陵岭领另令溜琉榴硫馏留刘瘤流柳六龙聋咙笼窿隆垄拢陇楼娄搂篓漏陋芦卢颅庐炉掳卤虏鲁麓碌露路赂鹿潞禄录陆戮驴吕铝侣旅履屡缕虑氯律率滤绿峦挛孪滦卵乱掠略抡轮伦仑沦纶论萝螺罗逻锣箩骡裸落洛骆络"),
    ('m', "妈麻玛码蚂马骂嘛吗埋买麦卖迈脉瞒馒蛮满蔓曼慢漫谩芒茫盲氓忙莽猫茅锚毛矛铆卯茂冒帽貌贸么玫枚梅酶霉煤没眉媒镁每美昧寐妹媚门闷们萌蒙檬盟锰猛梦孟眯醚靡迷谜弥米秘觅泌蜜密幂棉眠绵冕免勉娩缅面苗描瞄藐秒渺庙妙蔑灭民抿皿敏悯闽明螟鸣铭名命谬摸摹蘑模膜磨摩魔抹末莫墨默沫漠寞陌谋牟某母亩牡姆拇木目睦牧穆"),
    ('n', "拿哪呐钠那娜纳氖乃奶耐奈南男难囊挠脑恼闹淖呢馁内嫩能妮霓倪泥尼拟你匿腻逆溺蔫拈年碾撵捻念娘酿鸟尿捏聂孽啮镊镍涅您柠狞凝宁拧泞牛扭钮纽脓浓农弄奴努怒女暖虐疟挪懦糯诺"),
    ('o', "哦欧鸥殴藕呕偶沤"),
    ('p', "趴啪爬帕怕琶拍排牌徘湃派攀潘盘磐盼畔判叛乓庞旁耪胖抛咆刨炮袍跑泡呸胚培裴赔陪配佩沛喷盆砰抨烹澎彭蓬棚硼篷膨朋鹏捧碰坯砒霹批披劈琵毗啤脾疲皮匹痞僻屁譬篇偏片骗飘漂瓢票撇瞥拼频贫品聘乒坪苹萍平凭瓶评屏坡泼颇婆破魄迫粕剖扑铺仆莆葡菩蒲埔朴圃普浦谱曝瀑"),
    ('q', "期欺栖戚妻七凄漆柒沏其棋奇歧畦崎脐齐旗祈祁骑起岂乞企启契砌器气迄弃汽泣讫掐恰洽牵扦钎铅千迁签仟谦乾黔钱钳前潜遣浅谴堑嵌欠歉枪呛腔羌墙蔷强抢橇锹敲悄桥瞧乔侨巧鞘撬翘峭俏窍切茄且怯窃钦侵亲秦琴勤芹擒禽寝沁青轻氢倾卿清擎晴氰情顷请庆琼穷秋丘邱球求囚酋泅趋区蛆曲躯屈驱渠取娶龋趣去圈颧权醛泉全痊拳犬券劝缺炔瘸却鹊榷确雀裙群"),
    ('r', "然燃冉染瓤壤壤嚷让饶扰绕惹热壬仁人忍韧任认刃妊纫扔仍日戎茸蓉荣融熔溶容绒冗揉柔肉茹蠕儒孺如辱乳汝入褥软阮蕊瑞锐闰润若弱"),
    ('s', "撒洒萨腮鳃塞赛三叁伞散桑嗓丧搔骚扫嫂瑟色涩森僧莎砂杀刹沙纱傻啥煞筛晒珊苫杉山删煽衫闪陕擅赡膳善汕扇缮墒伤商赏晌上尚裳梢捎稍烧芍勺韶少哨邵绍奢赊蛇舌舍赦摄射慑涉社设砷申呻伸身深娠绅神沈审婶甚肾慎渗声生甥牲升绳省盛剩胜圣师失狮施湿诗尸虱十石拾时什食蚀实识史矢使屎驶始式示士世柿事拭誓逝势是嗜噬适仕侍释饰氏市恃室视试收手首守寿授售受瘦兽蔬枢梳殊抒输叔舒淑疏书赎孰熟薯暑曙署蜀黍鼠属术述树束戍竖墅庶数漱恕刷耍摔衰甩帅栓拴霜双爽谁水睡税吮瞬顺舜说硕朔烁斯撕嘶思私司丝死肆寺嗣四伺似饲巳松耸怂颂送宋讼诵搜艘擞嗽苏酥俗素速粟僳塑溯宿诉肃酸蒜算虽隋随绥髓碎岁穗遂隧祟孙损笋蓑梭唆缩琐索锁所"),
    ('t', "塌他它她塔獭挞蹋踏胎苔抬台泰酞太态汰坍摊贪瘫滩坛檀痰潭谭谈坦毯袒碳探叹炭汤塘搪堂棠膛唐糖倘躺淌趟烫掏涛滔绦萄桃逃淘陶讨套特藤腾疼誊梯剔踢锑提题蹄啼体替嚏惕涕剃屉天添填田甜恬舔腆挑条迢眺跳贴铁帖厅听烃汀廷停亭庭挺艇通桐酮瞳同铜彤童桶捅筒统痛偷投头透凸秃突图徒途涂屠土吐兔湍团推颓腿蜕褪退吞屯臀拖托脱鸵陀驮驼椭妥拓唾"),
    ('w', "挖哇蛙洼娃瓦袜歪外豌弯湾玩顽丸烷完碗挽晚皖惋宛婉万腕汪王亡枉网往旺望忘妄威巍微危韦违桅围唯惟为潍维苇萎委伟伪尾纬未蔚味畏胃喂魏位渭谓尉慰卫瘟温蚊文闻纹吻稳紊问嗡翁瓮挝蜗涡窝我斡卧握沃巫呜钨乌污诬屋无芜梧吾吴毋武五捂午舞伍侮坞戊雾晤物勿务悟误"),
    ('x', "昔熙析西硒矽晰嘻吸锡牺稀息希悉膝夕惜熄烯溪汐犀檄袭席习媳喜铣洗系隙戏细瞎虾匣霞辖暇峡侠狭下厦夏吓掀锨先仙鲜纤咸贤衔舷闲涎弦嫌显险现献县腺馅羡宪陷限线相厢镶香箱襄湘乡翔祥详想响享项巷橡像向象萧硝霄削哮嚣销消宵淆晓小孝校肖啸笑效楔些歇蝎鞋协挟携邪斜胁谐写械卸蟹懈泄泻谢屑薪芯锌欣辛新忻心信衅星腥猩惺兴刑型形邢行醒幸杏性姓兄凶胸匈汹雄熊休修羞朽嗅锈秀袖绣墟戌需虚嘘须徐许蓄酗叙旭序畜恤絮婿绪续轩喧宣悬旋玄选癣眩绚靴薛学穴雪血勋熏循旬询寻驯巡殉汛训讯逊迅"),
    ('y', "压押鸦鸭呀丫芽牙蚜崖衙涯雅哑亚讶焉咽阉烟淹盐严研蜒岩延言颜阎炎沿奄掩眼衍演艳堰燕厌砚雁唁彦焰宴谚验殃央鸯秧杨扬佯疡羊洋阳氧仰痒养样漾邀腰妖瑶摇尧遥窑谣姚咬舀药要耀椰噎耶爷野冶也页掖业叶曳腋夜液一壹医揖铱依伊衣颐夷遗移仪胰疑沂宜姨彝椅蚁倚已乙矣以艺抑易邑屹亿役臆逸肄疫亦裔意毅忆义益溢诣议谊译异翼翌绎茵荫因殷音阴姻吟银淫寅饮尹引隐印英樱婴鹰应缨莹萤营荧蝇迎赢盈影颖硬映哟拥佣臃痈庸雍踊蛹咏泳涌永恿勇用幽优悠忧尤由邮铀犹油游酉有友右佑釉诱又幼迂淤于盂榆虞愚舆余俞逾鱼愉渝渔隅予娱雨与屿禹宇语羽玉域芋郁吁遇喻峪御愈欲狱育誉浴寓裕预豫驭鸳渊冤元垣袁原援辕园员圆猿源缘远苑愿怨院曰约越跃钥岳粤月悦阅耘云郧匀陨允运蕴酝晕韵孕"),
    ('z', "匝砸杂栽哉灾宰载再在咱攒暂赞赃脏葬遭糟凿藻枣早澡蚤躁噪造皂灶燥责择则泽贼怎增憎曾赠扎喳渣札轧铡闸眨栅榨咋乍炸诈摘斋宅窄债寨瞻毡詹粘沾盏斩辗崭展蘸栈占战站湛绽樟章彰漳张掌涨杖丈帐账仗胀瘴障招昭找沼赵照罩兆肇召遮折哲蛰辙者锗蔗这浙珍斟真甄砧臻贞针侦枕疹诊断震振镇阵蒸挣睁征狰争怔整拯正政帧症郑证芝枝支吱蜘知肢脂汁之织职直植殖执值侄址指止趾只旨纸志挚掷至致置帜峙制智秩稚质炙痔滞治窒中盅忠钟衷终种肿重仲众舟周州洲诌粥轴肘帚咒皱宙昼骤珠株蛛朱猪诸诛逐竹烛煮拄瞩嘱主著柱助蛀贮铸筑住注祝驻抓爪拽专砖转撰赚篆桩庄装妆撞壮状椎锥追赘坠缀谆准捉拙卓桌琢茁酌啄着灼浊兹咨资姿滋淄孜紫仔籽滓子自渍字鬃踪宗综总纵邹走奏揍租足卒族祖诅阻组钻纂嘴醉最罪尊遵昨左佐柞做作坐座"),
];

#[inline]
fn table() -> &'static PinyinTable {
    PINYIN_TABLE.get_or_init(|| {
        let lut = vec![0u8; CJK_LEN].into_boxed_slice();
        let lut_arr: Box<[u8; CJK_LEN]> = lut.try_into().expect("CJK_LEN exact");
        let mut cjk_lut = lut_arr;
        let mut extras = Vec::new();
        for &(initial, chars) in BUCKETS {
            let b = initial as u8;
            for ch in chars.chars() {
                let code = ch as usize;
                if (CJK_BASE..=CJK_END).contains(&code) {
                    let slot = &mut cjk_lut[code - CJK_BASE];
                    if *slot == 0 {
                        *slot = b;
                    }
                } else if !extras.iter().any(|&(c, _)| c == ch) {
                    extras.push((ch, b));
                }
            }
        }
        PinyinTable { cjk_lut, extras }
    })
}

#[inline]
fn lookup_pinyin_initial(tbl: &PinyinTable, ch: char) -> Option<u8> {
    let code = ch as usize;
    if (CJK_BASE..=CJK_END).contains(&code) {
        let v = tbl.cjk_lut[code - CJK_BASE];
        if v != 0 {
            return Some(v);
        }
        return None;
    }
    tbl.extras
        .iter()
        .find_map(|&(c, b)| if c == ch { Some(b) } else { None })
}

/// 提取字符串的拼音首字母串（ASCII 字母转小写，数字原样保留，未收录汉字略过）。
pub fn to_pinyin_initials(text: &str) -> String {
    let tbl = table();
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if let Some(py) = lookup_pinyin_initial(tbl, ch) {
            out.push(py as char);
        }
    }
    out
}

/// 预编译的模糊与拼音检索条件：单次构建后可在成千上万个文件名上零分配高速复用。
#[derive(Debug, Clone)]
pub struct CompiledQuery {
    lower: String,
    is_ascii: bool,
    is_ascii_alnum: bool,
    char_count: usize,
}

impl CompiledQuery {
    pub fn new(raw: &str) -> Self {
        let trimmed = raw.trim();
        let lower = if trimmed.is_ascii() {
            trimmed.to_ascii_lowercase()
        } else {
            trimmed.to_lowercase()
        };
        let is_ascii = lower.is_ascii();
        let is_ascii_alnum = is_ascii && lower.bytes().all(|b| b.is_ascii_alphanumeric());
        let char_count = if is_ascii {
            lower.len()
        } else {
            lower.chars().count()
        };
        Self {
            lower,
            is_ascii,
            is_ascii_alnum,
            char_count,
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.lower.is_empty()
    }

    /// 匹配评分（越小越优先，`None` 表示不匹配）：
    /// 0 = 完全一致；1 = 前缀匹配；2 = 子串包含；3 = 拼音首字母前缀；4 = 拼音首字母包含；5 = 顺序模糊子序列。
    #[inline]
    pub fn match_rank(&self, text: &str) -> Option<u32> {
        let q = self.lower.as_str();
        if q.is_empty() {
            return Some(10);
        }

        // 零堆分配 ASCII 快路径（覆盖绝大多数源码、配置、系统文件与英文目录名）
        if self.is_ascii && text.is_ascii() {
            return match_ascii_only(text.as_bytes(), q.as_bytes(), self.is_ascii_alnum);
        }

        // 含中文或其他非 ASCII 字符路径
        if !self.is_ascii || !text.is_ascii() {
            // 若查询本身是纯 ASCII（如拼音首字母 "cwjdbb" 或扩展名 ".xlsx"）：
            // 先在原始字节上做 ASCII 不区分大小写子串扫描，无需分配 text.to_lowercase()！
            if self.is_ascii {
                let tb = text.as_bytes();
                let qb = q.as_bytes();
                if tb.len() == qb.len() && tb.eq_ignore_ascii_case(qb) {
                    return Some(0);
                }
                if tb.len() >= qb.len() && tb[..qb.len()].eq_ignore_ascii_case(qb) {
                    return Some(1);
                }
                if contains_ascii_case_insensitive(tb, qb) {
                    return Some(2);
                }
                if self.is_ascii_alnum
                    && let Some(py_rank) = match_pinyin_initials_noalloc(text, qb)
                {
                    return Some(py_rank);
                }
                if qb.len() >= 2 && is_ascii_subsequence_in_utf8(tb, qb) {
                    return Some(5);
                }
                return None;
            }
        }

        let t_low = text.to_lowercase();
        if t_low == q {
            return Some(0);
        }
        if t_low.starts_with(q) {
            return Some(1);
        }
        if t_low.contains(q) {
            return Some(2);
        }

        if self.is_ascii_alnum
            && let Some(py_rank) = match_pinyin_initials_noalloc(text, q.as_bytes())
        {
            return Some(py_rank);
        }

        if self.char_count >= 2 && is_subsequence(&t_low, q) {
            return Some(5);
        }
        None
    }
}

/// 匹配评分（单次查询便捷入口；批量扫描目录请优先使用 [`CompiledQuery`]）。
#[inline]
pub fn match_rank(text: &str, query: &str) -> Option<u32> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Some(10);
    }
    // 栈上零分配快路径：当 query 为 <= 64 字节 ASCII 时，彻底免去 query String 堆分配
    if trimmed.is_ascii() && trimmed.len() <= 64 {
        let mut q_buf = [0u8; 64];
        let qb = &mut q_buf[..trimmed.len()];
        let mut all_alnum = true;
        for (dst, &src) in qb.iter_mut().zip(trimmed.as_bytes()) {
            *dst = src.to_ascii_lowercase();
            if !dst.is_ascii_alphanumeric() {
                all_alnum = false;
            }
        }
        if text.is_ascii() {
            return match_ascii_only(text.as_bytes(), qb, all_alnum);
        }
        let tb = text.as_bytes();
        if contains_ascii_case_insensitive(tb, qb) {
            return Some(2);
        }
        if all_alnum && let Some(py_rank) = match_pinyin_initials_noalloc(text, qb) {
            return Some(py_rank);
        }
        if qb.len() >= 2 && is_ascii_subsequence_in_utf8(tb, qb) {
            return Some(5);
        }
        return None;
    }
    CompiledQuery::new(trimmed).match_rank(text)
}

#[inline]
fn match_ascii_only(tb: &[u8], qb: &[u8], q_alnum: bool) -> Option<u32> {
    let t_len = tb.len();
    let q_len = qb.len();
    if t_len >= q_len {
        if t_len == q_len && tb.eq_ignore_ascii_case(qb) {
            return Some(0);
        }
        if tb[..q_len].eq_ignore_ascii_case(qb) {
            return Some(1);
        }
        if contains_ascii_case_insensitive(tb, qb) {
            return Some(2);
        }
    }

    // 对纯 ASCII 文本，to_pinyin_initials 等价于抽取出所有 ASCII 字母数字（小写）
    if q_alnum && t_len >= q_len {
        let mut stack_init = [0u8; 128];
        if t_len <= stack_init.len() {
            let mut n = 0usize;
            for &b in tb {
                if b.is_ascii_alphanumeric() {
                    stack_init[n] = b.to_ascii_lowercase();
                    n += 1;
                }
            }
            let init = &stack_init[..n];
            if init.len() >= q_len {
                if &init[..q_len] == qb {
                    return Some(3);
                }
                if contains_exact_bytes(init, qb) {
                    return Some(4);
                }
            }
        } else {
            let mut vec_init = Vec::with_capacity(t_len);
            for &b in tb {
                if b.is_ascii_alphanumeric() {
                    vec_init.push(b.to_ascii_lowercase());
                }
            }
            if vec_init.len() >= q_len {
                if &vec_init[..q_len] == qb {
                    return Some(3);
                }
                if contains_exact_bytes(&vec_init, qb) {
                    return Some(4);
                }
            }
        }
    }

    if q_len >= 2 && is_ascii_subsequence_in_utf8(tb, qb) {
        return Some(5);
    }
    None
}

#[inline]
fn contains_ascii_case_insensitive(haystack: &[u8], needle_lower: &[u8]) -> bool {
    let n_len = needle_lower.len();
    if n_len == 0 {
        return true;
    }
    if haystack.len() < n_len {
        return false;
    }
    let first_lo = needle_lower[0];
    let first_hi = first_lo.to_ascii_uppercase();
    let rest = &needle_lower[1..];
    let max_start = haystack.len() - n_len;
    let mut i = 0usize;
    while i <= max_start {
        let b = haystack[i];
        if (b == first_lo || b == first_hi) && haystack[i + 1..i + n_len].eq_ignore_ascii_case(rest)
        {
            return true;
        }
        i += 1;
    }
    false
}

#[inline]
fn contains_exact_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    let n_len = needle.len();
    if n_len == 0 {
        return true;
    }
    if haystack.len() < n_len {
        return false;
    }
    let first = needle[0];
    let max_start = haystack.len() - n_len;
    let mut i = 0usize;
    while i <= max_start {
        if haystack[i] == first && &haystack[i..i + n_len] == needle {
            return true;
        }
        i += 1;
    }
    false
}

#[inline]
fn match_pinyin_initials_noalloc(text: &str, qb_lower: &[u8]) -> Option<u32> {
    let tbl = table();
    let mut stack_buf = [0u8; 128];
    if text.len() <= stack_buf.len() {
        let mut n = 0usize;
        for ch in text.chars() {
            if ch.is_ascii_alphanumeric() {
                stack_buf[n] = ch.to_ascii_lowercase() as u8;
                n += 1;
            } else if let Some(py) = lookup_pinyin_initial(tbl, ch) {
                stack_buf[n] = py;
                n += 1;
            }
        }
        let init = &stack_buf[..n];
        if init.len() >= qb_lower.len() {
            if &init[..qb_lower.len()] == qb_lower {
                return Some(3);
            }
            if contains_exact_bytes(init, qb_lower) {
                return Some(4);
            }
        }
        None
    } else {
        let initials = to_pinyin_initials(text);
        let init = initials.as_bytes();
        if init.len() >= qb_lower.len() {
            if &init[..qb_lower.len()] == qb_lower {
                return Some(3);
            }
            if contains_exact_bytes(init, qb_lower) {
                return Some(4);
            }
        }
        None
    }
}

#[inline]
fn is_ascii_subsequence_in_utf8(haystack: &[u8], needle_lower: &[u8]) -> bool {
    if haystack.len() < needle_lower.len() {
        return false;
    }
    let mut qi = 0usize;
    let mut target = needle_lower[0];
    for &b in haystack {
        if b < 0x80 && b.to_ascii_lowercase() == target {
            qi += 1;
            if qi == needle_lower.len() {
                return true;
            }
            target = needle_lower[qi];
        }
    }
    false
}

fn is_subsequence(haystack: &str, needle: &str) -> bool {
    let mut it = haystack.chars();
    for nc in needle.chars() {
        if !it.any(|hc| hc == nc) {
            return false;
        }
    }
    true
}
