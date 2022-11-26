#![allow(clippy::redundant_clone)]
#![feature(custom_inner_attributes)]

fn main() {}

fn just_under_msrv() {
    #![clippy::msrv = "1.42.0"]
    let log2_10 = 3.321928094887362;
}

fn meets_msrv() {
    #![clippy::msrv = "1.43.0"]
    let log2_10 = 3.321928094887362;
}

fn just_above_msrv() {
    #![clippy::msrv = "1.44.0"]
    let log2_10 = 3.321928094887362;
}

fn no_patch_under() {
    #![clippy::msrv = "1.42"]
    let log2_10 = 3.321928094887362;
}

fn no_patch_meets() {
    #![clippy::msrv = "1.43"]
    let log2_10 = 3.321928094887362;
}

// https://github.com/rust-lang/rust-clippy/issues/6920
fn scoping() {
    mod m {
        #![clippy::msrv = "1.42.0"]
    }

    // Should warn
    let log2_10 = 3.321928094887362;

    mod a {
        #![clippy::msrv = "1.42.0"]

        fn should_warn() {
            #![clippy::msrv = "1.43.0"]
            let log2_10 = 3.321928094887362;
        }

        fn should_not_warn() {
            let log2_10 = 3.321928094887362;
        }
    }
}
