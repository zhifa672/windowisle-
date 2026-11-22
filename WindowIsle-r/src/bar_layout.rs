//! 常驻组件按实际占位排布，并在开关变化时连续补位。
use crate::anim::{smoothstep, Spring};

pub(crate) const CLOCK: usize = 0;
pub(crate) const BATTERY: usize = 1;
pub(crate) const QUOTA: usize = 2;
pub(crate) const MORE: usize = 5;
pub(crate) const MUSIC: usize = 6;
pub(crate) const FOCUS: usize = 7;
pub(crate) const BELL: usize = 8;
pub(crate) const PET: usize = 9;
/// 内存清理：收起态右侧的内存占用圆环（点击清理）。
pub(crate) const MEMORY: usize = 10;
/// Agent 任务：收起态的任务进度环（点击查看）。
pub(crate) const TASK: usize = 11;
const WIDTHS: [f32; 12] = [44.0,20.0,27.0,27.0,27.0,22.0,14.0,crate::focus_bar::WIDTH,20.0,32.0,16.0,46.0];

/// 时钟显示到秒时药丸加宽（"HH:MM:SS" 在 15.5 号 Bold 下约 60px）。
static CLOCK_WIDE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn set_clock_wide(on: bool) {
    CLOCK_WIDE.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// 槽位宽度（时钟按设置动态加宽）。
pub(crate) fn slot_width(slot: usize) -> f32 {
    if slot == CLOCK && CLOCK_WIDE.load(std::sync::atomic::Ordering::Relaxed) {
        60.0
    } else {
        WIDTHS[slot]
    }
}

pub(crate) const DEFAULT_ORDER: [usize;8]=[BATTERY,PET,CLOCK,QUOTA,TASK,MEMORY,MUSIC,FOCUS];
pub(crate) fn valid_order(order:&[usize])->bool {
    order.len()==8 && DEFAULT_ORDER.iter().all(|slot|order.iter().filter(|value|*value==slot).count()==1)
}

/// 用户外观设置对岛体动效的覆盖：-1 跟随系统，0 关闭，1 开启。
static MOTION_OVERRIDE: std::sync::atomic::AtomicI8 = std::sync::atomic::AtomicI8::new(-1);

pub(crate) fn set_motion_override(enabled: Option<bool>) {
    MOTION_OVERRIDE.store(
        match enabled {
            None => -1,
            Some(false) => 0,
            Some(true) => 1,
        },
        std::sync::atomic::Ordering::Relaxed,
    );
}

pub(crate) fn animations_enabled()->bool {
    match MOTION_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => true,
        0 => false,
        _ => system_animations_enabled(),
    }
}

fn system_animations_enabled()->bool {
    #[cfg(windows)] {
        static ENABLED:std::sync::OnceLock<bool>=std::sync::OnceLock::new();
        *ENABLED.get_or_init(||{
            use windows::Win32::{Foundation::BOOL,UI::WindowsAndMessaging::*};
            let mut enabled=BOOL(1);
            unsafe {let _=SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION,0,Some((&mut enabled as *mut BOOL).cast()),SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));}
            enabled.as_bool()
        })
    }
    #[cfg(not(windows))] {true}
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Layout {
    pub width: f32,
    pub x: [Option<f32>; 12],
}

impl Layout {
    pub fn new(battery:bool,quotas:usize,music:bool,focus:bool,bell:bool,pet:bool,memory:bool,task:bool)->Self {
        fn build(battery:bool,quotas:usize,music:bool,focus:bool,bell:bool,pet:bool,memory:bool,task:bool)->Layout {
            fn place(cursor:&mut f32,width:f32)->f32 {let x=*cursor+width/2.0;*cursor+=width+12.0;x}
            let mut x=[None;12];let mut cursor=16.0;
            let count=quotas.min(3);
            let order=if battery {[BATTERY,CLOCK,QUOTA]}else{[QUOTA,CLOCK,BATTERY]};
            for slot in order {
                match slot {
                    BATTERY if battery=>x[slot]=Some(place(&mut cursor,slot_width(slot))),
                    CLOCK=>{
                        if pet {x[PET]=Some(place(&mut cursor,slot_width(PET)));}
                        x[slot]=Some(place(&mut cursor,slot_width(slot)));
                    },
                    QUOTA if count>0=>{
                        let width=count as f32*27.0+(count-1) as f32*7.0+if quotas>3{29.0}else{0.0};
                        let left=place(&mut cursor,width)-width/2.0;
                        for i in 0..count {x[QUOTA+i]=Some(left+13.5+i as f32*34.0);}
                        if quotas>3 {x[MORE]=Some(left+count as f32*34.0+11.0);}
                    },
                    _=>{},
                }
            }
            for (slot,on) in [(TASK,task),(MEMORY,memory),(MUSIC,music&&!bell),(FOCUS,focus),(BELL,bell)] {
                if on {x[slot]=Some(place(&mut cursor,slot_width(slot)));}
            }
            let width=cursor-12.0+16.0;
            for value in x.iter_mut().flatten() {*value-=width/2.0;}
            Layout {width:width.max(96.0),x}
        }
        let layout=build(battery,quotas,music,focus,bell,pet,memory,task);
        // 组件太多挤不下时依次让位：先收内存圆环（信息性组件），还放不下再收任务槽位。
        if layout.width<=crate::app::PW {return layout;}
        let without_memory=build(battery,quotas,music,focus,bell,pet,false,task);
        if !task || without_memory.width<=crate::app::PW {return without_memory;}
        build(battery,quotas,music,focus,bell,pet,false,false)
    }
    pub fn reorder(&mut self,order:&[usize]) {
        if !valid_order(order){return;}
        let original=self.x;let mut cursor=-self.width/2.0+16.0;
        for slot in order.iter().copied().chain(std::iter::once(BELL)) {
            let slots:Vec<usize>=if slot==QUOTA{(QUOTA..=MORE).collect()}else{vec![slot]};
            let left=slots.iter().filter_map(|&i|original[i].map(|x|x-slot_width(i)/2.0)).fold(f32::INFINITY,f32::min);
            let right=slots.iter().filter_map(|&i|original[i].map(|x|x+slot_width(i)/2.0)).fold(f32::NEG_INFINITY,f32::max);
            if !left.is_finite(){continue;}
            for i in slots {if let Some(x)=original[i]{self.x[i]=Some(cursor+x-left);}}
            cursor+=right-left+12.0;
        }
    }
    /// 圆环换边经过时钟时，暂时淡出时钟，避免移动中相互遮挡。
    pub fn clearance(&self,slot:usize)->f32 {
        let Some(x)=self.x[slot] else{return 0.0;};
        let distance=self.x[QUOTA..=MORE].iter().flatten().map(|other|(other-x).abs()).fold(f32::INFINITY,f32::min);
        let quota=if slot==CLOCK {smoothstep(34.0,44.0,distance)}else{smoothstep(22.0,34.0,distance)};
        quota*self.pet_clearance(slot)
    }
    pub fn pet_clearance(&self,slot:usize)->f32 {
        match (self.x[slot],self.x[PET]) {
            (Some(x),Some(pet))=>smoothstep(slot_width(slot)/2.0+14.0,slot_width(slot)/2.0+24.0,(x-pet).abs()),
            _=>1.0,
        }
    }
}

pub(crate) struct Motion {
    pub target: Layout,
    positions: [Spring; 12],
    initialized: bool,
}
impl Default for Motion {
    fn default()->Self {Self {target:Layout::new(false,0,false,false,false,false,false,false),positions:[Spring::new(0.0,360.0,0.76);12],initialized:false}}
}
impl Motion {
    pub fn step(&mut self,target:Layout,dt:f32,animate:bool) {
        for (i,x) in target.x.iter().enumerate() {
            if let Some(x)=x {
                let spring=&mut self.positions[i];spring.target=*x;
                if !self.initialized || self.target.x[i].is_none() || !animate {spring.snap(*x);}else{spring.step(dt);}
            }
        }
        self.target=target;self.initialized=true;
    }
    pub fn current(&self)->Layout {
        let mut layout=self.target;
        for (i,x) in layout.x.iter_mut().enumerate() {if x.is_some(){*x=Some(self.positions[i].pos);}}
        layout
    }
    pub fn position(&self,slot:usize)->f32 {self.positions[slot].pos}
    pub fn settled(&self)->bool {self.target.x.iter().enumerate().all(|(i,x)|x.is_none()||self.positions[i].settled(100.0))}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn custom_order_preserves_groups_and_hidden_positions(){
        assert!(!valid_order(&[0,0,2,6,7,9]));
        assert!(!valid_order(&[0,1,2]));
        let order=[FOCUS,MUSIC,MEMORY,TASK,QUOTA,CLOCK,PET,BATTERY];
        for battery in [false,true]{for count in 0..=5 {
            let mut layout=Layout::new(battery,count,true,true,false,true,false,false);
            let width=layout.width;layout.reorder(&order);
            assert_eq!(layout.width,width);
            let positions:Vec<_>=order.iter().filter_map(|i|layout.x[*i]).collect();
            assert!(positions.windows(2).all(|p|p[0]<p[1]));
            if count>1 {assert_eq!(layout.x[QUOTA+1].unwrap()-layout.x[QUOTA].unwrap(),34.0);}
            let mut boxes:Vec<_>=layout.x.iter().enumerate().filter_map(|(i,x)|x.map(|x|(x-WIDTHS[i]/2.0,x+WIDTHS[i]/2.0))).collect();
            boxes.sort_by(|a,b|a.0.total_cmp(&b.0));
            assert!(boxes.windows(2).all(|p|p[1].0-p[0].1>=6.9));
        }}
    }
    #[test]
    fn every_component_combination_fits_without_overlaps(){
        for quotas in 0..=8 {for battery in [false,true] {for music in [false,true] {for focus in [false,true] {for bell in [false,true] {for pet in [false,true] {for memory in [false,true] {for task in [false,true] {
            let layout=Layout::new(battery,quotas,music,focus,bell,pet,memory,task);
            let mut boxes:Vec<_>=layout.x.iter().enumerate().filter_map(|(i,x)|x.map(|x|(x-WIDTHS[i]/2.0,x+WIDTHS[i]/2.0))).collect();
            boxes.sort_by(|a,b|a.0.total_cmp(&b.0));
            assert!(layout.width<=crate::app::PW);
            assert!(boxes.first().unwrap().0>=-layout.width/2.0+15.9);
            assert!(boxes.last().unwrap().1<=layout.width/2.0-15.9);
            assert!(boxes.windows(2).all(|pair|pair[1].0-pair[0].1>=6.9),"组件不能挤在一起：{layout:?}");
            assert_eq!(layout.clearance(CLOCK),1.0);
        }}}}}}}}
    }
    #[test]
    fn replacing_battery_moves_continuously_and_supports_an_instant_state(){
        let before=Layout::new(true,1,true,false,false,true,false,false);let after=Layout::new(false,1,true,false,false,true,false,false);
        let mut motion=Motion::default();motion.step(before,0.0,true);
        motion.step(after,1.0/60.0,true);let mid=motion.current().x[QUOTA].unwrap();
        assert!(mid<before.x[QUOTA].unwrap()&&mid>after.x[QUOTA].unwrap());
        for _ in 0..120 {motion.step(after,1.0/60.0,true);}
        assert!((motion.current().x[QUOTA].unwrap()-after.x[QUOTA].unwrap()).abs()<0.01);
        motion.step(before,1.0/60.0,false);assert_eq!(motion.current().x,before.x);
    }
}
