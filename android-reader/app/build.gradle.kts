plugins {
    id("com.android.application")
}

android {
    namespace = "com.nuedc.reader"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.nuedc.reader"
        minSdk = 26
        targetSdk = 35
        versionCode = 9
        versionName = "1.7.0"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro"
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}
